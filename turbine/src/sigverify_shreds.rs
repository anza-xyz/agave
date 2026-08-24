use {
    crate::{
        cluster_nodes::{ClusterNodesCache, DATA_PLANE_FANOUT},
        retransmit_stage::RetransmitStage,
    },
    agave_feature_set as feature_set,
    crossbeam_channel::{Receiver, RecvTimeoutError, SendError, Sender},
    rayon::{ThreadPool, ThreadPoolBuilder, prelude::*},
    solana_gossip::cluster_info::ClusterInfo,
    solana_keypair::Keypair,
    solana_ledger::{
        blockstore_meta::BlockLocation,
        leader_schedule_cache::LeaderScheduleCache,
        shred::{
            self,
            layout::{get_shred, resign_packet},
            wire::is_retransmitter_signed_variant,
        },
        sigverify_shreds::verify_shred_with_leader,
    },
    solana_perf::{
        self,
        deduper::Deduper,
        packet::{PacketBatch, PacketRef, PacketRefMut},
    },
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    solana_signer::Signer,
    solana_streamer::{evicting_sender::EvictingSender, streamer::ChannelSend},
    std::{
        num::NonZeroUsize,
        sync::{
            Arc, RwLock,
            atomic::{AtomicUsize, Ordering},
        },
        thread::{Builder, JoinHandle},
        time::{Duration, Instant},
    },
    thiserror::Error,
};

const DEDUPER_FALSE_POSITIVE_RATE: f64 = 0.001;
const DEDUPER_NUM_BITS: u64 = 637_534_199; // 76MB
const DEDUPER_RESET_CYCLE: Duration = Duration::from_secs(5 * 60);

// Num epochs capacity should be at least 2 because near the epoch boundary we
// may receive shreds from the other side of the epoch boundary. Because of the
// TTL based eviction it is extremely unlikely that we will ever store > 2 epochs anyway
const CLUSTER_NODES_CACHE_NUM_EPOCH_CAP: usize = 2;
// Because for ClusterNodes::get_retransmit_parent only pubkeys of staked nodes
// are needed, we can use longer durations for cache TTL.
const CLUSTER_NODES_CACHE_TTL: Duration = Duration::from_secs(30);

/// Maximum number of packet batches to process in a single sigverify iteration.
const SIGVERIFY_SHRED_BATCH_SIZE: usize = 1024;

#[allow(clippy::enum_variant_names)]
enum ShredSigverifyError {
    RecvDisconnected,
    RecvTimeout,
    SendError,
}

#[derive(Debug, Error)]
enum ResignError {
    #[error("verification of retransmitter signature failed")]
    VerifyRetransmitterSignature,
    #[error(transparent)]
    Shred(#[from] shred::Error),
}

pub type RepairNonceLocationLookup = dyn Fn(shred::Nonce) -> Option<BlockLocation> + Send + Sync;

pub fn spawn_shred_sigverify(
    cluster_info: Arc<ClusterInfo>,
    bank_forks: Arc<RwLock<BankForks>>,
    leader_schedule_cache: Arc<LeaderScheduleCache>,
    shred_fetch_receiver: Receiver<PacketBatch>,
    retransmit_sender: EvictingSender<Vec<shred::Payload>>,
    verified_sender: Sender<Vec<(shred::Payload, /*is_repaired:*/ bool, BlockLocation)>>,
    repair_nonce_location_lookup: Arc<RepairNonceLocationLookup>,
    num_sigverify_threads: NonZeroUsize,
) -> JoinHandle<()> {
    let mut stats = ShredSigVerifyStats::new(Instant::now());
    let cluster_nodes_cache = ClusterNodesCache::<RetransmitStage>::new(
        CLUSTER_NODES_CACHE_NUM_EPOCH_CAP,
        CLUSTER_NODES_CACHE_TTL,
    );
    let thread_pool = ThreadPoolBuilder::new()
        .num_threads(num_sigverify_threads.get())
        .thread_name(|i| format!("solSvrfyShred{i:02}"))
        .build()
        .expect("new rayon threadpool");
    let run_shred_sigverify = move || {
        let mut rng = rand::rng();
        let deduper = Deduper::<2, [u8]>::new(&mut rng, DEDUPER_NUM_BITS);
        let mut shred_buffer = Vec::with_capacity(SIGVERIFY_SHRED_BATCH_SIZE);
        loop {
            if deduper.maybe_reset(&mut rng, DEDUPER_FALSE_POSITIVE_RATE, DEDUPER_RESET_CYCLE) {
                stats.num_deduper_saturations += 1;
            }
            // We can't store the keypair outside the loop
            // because the identity might be hot swapped.
            let keypair = cluster_info.keypair();
            match run_shred_sigverify(
                &thread_pool,
                &keypair,
                &cluster_info,
                &bank_forks,
                &leader_schedule_cache,
                &deduper,
                &shred_fetch_receiver,
                &retransmit_sender,
                &verified_sender,
                &cluster_nodes_cache,
                repair_nonce_location_lookup.as_ref(),
                &mut stats,
                &mut shred_buffer,
            ) {
                Ok(()) => (),
                Err(ShredSigverifyError::RecvTimeout) => (),
                Err(ShredSigverifyError::RecvDisconnected) => break,
                Err(ShredSigverifyError::SendError) => break,
            }
            stats.maybe_submit();
        }
    };
    Builder::new()
        .name("solShredVerifr".to_string())
        .spawn(run_shred_sigverify)
        .unwrap()
}

#[allow(clippy::too_many_arguments)]
fn run_shred_sigverify<const K: usize>(
    thread_pool: &ThreadPool,
    keypair: &Keypair,
    cluster_info: &ClusterInfo,
    bank_forks: &RwLock<BankForks>,
    leader_schedule_cache: &LeaderScheduleCache,
    deduper: &Deduper<K, [u8]>,
    shred_fetch_receiver: &Receiver<PacketBatch>,
    retransmit_sender: &EvictingSender<Vec<shred::Payload>>,
    verified_sender: &Sender<Vec<(shred::Payload, /*is_repaired:*/ bool, BlockLocation)>>,
    cluster_nodes_cache: &ClusterNodesCache<RetransmitStage>,
    repair_nonce_location_lookup: &RepairNonceLocationLookup,
    stats: &mut ShredSigVerifyStats,
    shred_buffer: &mut Vec<PacketBatch>,
) -> Result<(), ShredSigverifyError> {
    const RECV_TIMEOUT: Duration = Duration::from_secs(1);
    let packets = shred_fetch_receiver.recv_timeout(RECV_TIMEOUT)?;
    stats.num_packets += packets.len();
    shred_buffer.push(packets);
    for packets in shred_fetch_receiver
        .try_iter()
        .take(SIGVERIFY_SHRED_BATCH_SIZE - 1)
    {
        stats.num_packets += packets.len();
        shred_buffer.push(packets);
    }

    let now = Instant::now();
    stats.num_iters += 1;
    stats.num_batches += shred_buffer.len();
    stats.num_discards_pre += count_discards(shred_buffer);
    // Repair shreds include a randomly generated u32 nonce, so it does not
    // make sense to deduplicate the entire packet payload (i.e. they are not
    // duplicate of any other packet.data(..)).
    // If the nonce is excluded from the deduper then false positives might
    // prevent us from repairing a block until the deduper is reset after
    // DEDUPER_RESET_CYCLE. A workaround is to also repair "coding" shreds to
    // add some redundancy but that is not implemented at the moment.
    // Because the repair nonce is already verified in shred-fetch-stage we can
    // exclude repair shreds from the deduper, but we still need to pass the
    // repair shred to the deduper to filter out duplicates from the turbine
    // path once a shred is repaired.
    // For backward compatibility we need to allow trailing bytes in the packet
    // after the shred payload, but have to exclude them here from the deduper.
    let (working_bank, root_bank) = {
        let bank_forks = bank_forks.read().unwrap();
        (bank_forks.working_bank(), bank_forks.root_bank())
    };
    let self_pubkey = keypair.pubkey();
    let (
        num_duplicates,
        num_discards_post,
        resign_micros,
        num_unknown_block_location,
        shreds,
        repairs,
    ) = thread_pool.install(|| {
        shred_buffer
            .par_iter_mut()
            .flatten()
            .fold(
                || (0, 0, 0, 0, Vec::new(), Vec::new()),
                |(
                    mut num_duplicates_acc,
                    mut num_discards_post_acc,
                    mut resign_micros_acc,
                    mut num_unknown_block_location_acc,
                    mut shreds_acc,
                    mut repairs_acc,
                ),
                 mut packet| {
                    if packet.meta().discard() {
                        num_discards_post_acc += 1;
                        return (
                            num_duplicates_acc,
                            num_discards_post_acc,
                            resign_micros_acc,
                            num_unknown_block_location_acc,
                            shreds_acc,
                            repairs_acc,
                        );
                    }
                    let duplicate = shred::wire::get_shred(packet.as_ref())
                        .map(|shred| deduper.dedup(shred))
                        .unwrap_or(true);
                    if duplicate && !packet.meta().repair() {
                        packet.meta_mut().set_discard(true);
                        num_duplicates_acc += 1;
                    }
                    if !packet.meta().discard()
                        && !verify_packet_signature(
                            &self_pubkey,
                            packet.as_ref(),
                            &working_bank,
                            leader_schedule_cache,
                        )
                    {
                        packet.meta_mut().set_discard(true);
                    }
                    if packet.meta().discard() {
                        num_discards_post_acc += 1;
                        return (
                            num_duplicates_acc,
                            num_discards_post_acc,
                            resign_micros_acc,
                            num_unknown_block_location_acc,
                            shreds_acc,
                            repairs_acc,
                        );
                    }
                    let resign_start = Instant::now();
                    if maybe_verify_and_resign_packet(
                        &mut packet,
                        &root_bank,
                        &working_bank,
                        cluster_info,
                        leader_schedule_cache,
                        cluster_nodes_cache,
                        stats,
                        keypair,
                    )
                    .is_err()
                    {
                        packet.meta_mut().set_discard(true);
                    }
                    resign_micros_acc += resign_start.elapsed().as_micros() as u64;
                    if !packet.meta().discard()
                        && let Some((shred, nonce)) =
                            shred::layout::get_shred_and_repair_nonce(packet.as_ref())
                    {
                        let shred = shred::Payload::from(shred.to_vec());
                        match nonce {
                            None => {
                                // Share the payload between the retransmit-stage and the
                                // window-service.
                                shreds_acc.push(shred);
                            }
                            Some(nonce) => {
                                if let Some(location) = repair_nonce_location_lookup(nonce) {
                                    // No need for Arc overhead here because repaired shreds
                                    // are not retranmitted.
                                    repairs_acc
                                        .push((shred, /* is_repaired */ true, location));
                                } else {
                                    num_unknown_block_location_acc += 1;
                                }
                            }
                        }
                    }
                    (
                        num_duplicates_acc,
                        num_discards_post_acc,
                        resign_micros_acc,
                        num_unknown_block_location_acc,
                        shreds_acc,
                        repairs_acc,
                    )
                },
            )
            .reduce(
                || (0, 0, 0, 0, Vec::new(), Vec::new()),
                |(
                    num_duplicates_a,
                    num_discards_post_a,
                    resign_micros_a,
                    num_unknown_block_location_a,
                    mut shreds_a,
                    mut repairs_a,
                ),
                 (
                    num_duplicates_b,
                    num_discards_post_b,
                    resign_micros_b,
                    num_unknown_block_location_b,
                    shreds_b,
                    repairs_b,
                )| {
                    shreds_a.extend(shreds_b);
                    repairs_a.extend(repairs_b);
                    (
                        num_duplicates_a + num_duplicates_b,
                        num_discards_post_a + num_discards_post_b,
                        resign_micros_a + resign_micros_b,
                        num_unknown_block_location_a + num_unknown_block_location_b,
                        shreds_a,
                        repairs_a,
                    )
                },
            )
    });
    stats.num_duplicates += num_duplicates;
    stats.num_discards_post += num_discards_post;
    stats.resign_micros += resign_micros;
    stats.num_unknown_block_location += num_unknown_block_location;
    // Repaired shreds are not retransmitted.
    stats.num_retransmit_shreds += shreds.len();
    if let Err(send_err) = retransmit_sender.try_send(shreds.clone()) {
        match send_err {
            crossbeam_channel::TrySendError::Full(v) => {
                stats.num_retransmit_stage_overflow_shreds += v.len();
            }
            _ => unreachable!("EvictingSender holds on to both ends of the channel"),
        }
    }
    // Send all shreds to window service to be inserted into blockstore.
    let shreds = shreds
        .into_iter()
        .map(|shred| (shred, /*is_repaired:*/ false, BlockLocation::Original));
    verified_sender.send(shreds.chain(repairs).collect())?;
    stats.elapsed_micros += now.elapsed().as_micros() as u64;
    shred_buffer.clear();
    Ok(())
}

/// Checks whether the shred in the given `packet` is of resigned variant. If
/// yes, it calls [`verify_and_resign_shred`].
fn maybe_verify_and_resign_packet(
    packet: &mut PacketRefMut,
    root_bank: &Bank,
    working_bank: &Bank,
    cluster_info: &ClusterInfo,
    leader_schedule_cache: &LeaderScheduleCache,
    cluster_nodes_cache: &ClusterNodesCache<RetransmitStage>,
    stats: &ShredSigVerifyStats,
    keypair: &Keypair,
) -> Result<(), ResignError> {
    let repair = packet.meta().repair();
    let shred = get_shred(packet.as_ref()).ok_or(shred::Error::InvalidPacketSize)?;
    let is_signed = is_retransmitter_signed_variant(shred)?;
    if is_signed {
        // Repair packets do not follow turbine tree and
        // are verified using the trailing nonce.
        if !repair
            && !verify_retransmitter_signature(
                shred,
                root_bank,
                working_bank,
                cluster_info,
                leader_schedule_cache,
                cluster_nodes_cache,
                stats,
            )
        {
            stats
                .num_invalid_retransmitter
                .fetch_add(1, Ordering::Relaxed);
            if shred::layout::get_slot(shred)
                .map(|slot| {
                    shred::filter::check_feature_activation_from_bank(
                        &feature_set::verify_retransmitter_signature::id(),
                        slot,
                        root_bank,
                    )
                })
                .unwrap_or_default()
            {
                return Err(ResignError::VerifyRetransmitterSignature);
            }
        }

        resign_packet(packet, keypair)?;
    }

    Ok(())
}

#[must_use]
fn verify_retransmitter_signature(
    shred: &[u8],
    root_bank: &Bank,
    working_bank: &Bank,
    cluster_info: &ClusterInfo,
    leader_schedule_cache: &LeaderScheduleCache,
    cluster_nodes_cache: &ClusterNodesCache<RetransmitStage>,
    stats: &ShredSigVerifyStats,
) -> bool {
    let signature = match shred::layout::get_retransmitter_signature(shred) {
        Ok(signature) => signature,
        // If the shred is not of resigned variant,
        // then there is nothing to verify.
        Err(shred::Error::InvalidShredVariant) => return true,
        Err(_) => return false,
    };
    let Some(merkle_root) = shred::layout::get_merkle_root(shred) else {
        return false;
    };
    let Some(shred) = shred::layout::get_shred_id(shred) else {
        return false;
    };
    let Some(leader) = leader_schedule_cache.slot_leader_at(shred.slot(), Some(working_bank))
    else {
        stats
            .num_unknown_slot_leader
            .fetch_add(1, Ordering::Relaxed);
        return false;
    };
    let cluster_nodes =
        cluster_nodes_cache.get(shred.slot(), root_bank, working_bank, cluster_info);
    let parent = match cluster_nodes.get_retransmit_parent(&leader.id, &shred, DATA_PLANE_FANOUT) {
        Ok(Some(parent)) => parent,
        Ok(None) => {
            stats
                .num_retranmitter_signature_skipped
                .fetch_add(1, Ordering::Relaxed);
            return true;
        }
        Err(err) => {
            error!("get_retransmit_parent: {err:?}");
            stats
                .num_unknown_turbine_parent
                .fetch_add(1, Ordering::Relaxed);
            return false;
        }
    };
    if signature.verify(parent.as_ref(), merkle_root.as_ref()) {
        stats
            .num_retranmitter_signature_verified
            .fetch_add(1, Ordering::Relaxed);
        true
    } else {
        false
    }
}

fn verify_packet_signature(
    self_pubkey: &Pubkey,
    packet: PacketRef,
    working_bank: &Bank,
    leader_schedule_cache: &LeaderScheduleCache,
) -> bool {
    if packet.meta().discard() {
        return false;
    }
    let Some(shred) = shred::layout::get_shred(packet) else {
        return false;
    };
    let Some(slot) = shred::layout::get_slot(shred) else {
        return false;
    };
    let Some(leader) = leader_schedule_cache
        .slot_leader_at(slot, Some(working_bank))
        .map(|leader| leader.id)
        .filter(|leader| leader != self_pubkey)
    else {
        return false;
    };
    verify_shred_with_leader(packet, &leader)
}

fn count_discards(packets: &[PacketBatch]) -> usize {
    packets
        .iter()
        .flat_map(|batch| batch.iter())
        .filter(|packet| packet.meta().discard())
        .count()
}

impl From<RecvTimeoutError> for ShredSigverifyError {
    fn from(err: RecvTimeoutError) -> Self {
        match err {
            RecvTimeoutError::Timeout => Self::RecvTimeout,
            RecvTimeoutError::Disconnected => Self::RecvDisconnected,
        }
    }
}

impl<T> From<SendError<T>> for ShredSigverifyError {
    fn from(_: SendError<T>) -> Self {
        Self::SendError
    }
}

struct ShredSigVerifyStats {
    since: Instant,
    num_iters: usize,
    num_batches: usize,
    num_packets: usize,
    num_deduper_saturations: usize,
    num_discards_post: usize,
    num_discards_pre: usize,
    num_duplicates: usize,
    num_invalid_retransmitter: AtomicUsize,
    num_retranmitter_signature_skipped: AtomicUsize,
    num_retranmitter_signature_verified: AtomicUsize,
    num_retransmit_stage_overflow_shreds: usize,
    num_retransmit_shreds: usize,
    /// This means the OutstandingRequests cache is saturated and we
    /// threw away a verified shred due to being unable to fetch the storage location
    num_unknown_block_location: usize,
    num_unknown_slot_leader: AtomicUsize,
    num_unknown_turbine_parent: AtomicUsize,
    elapsed_micros: u64,
    resign_micros: u64,
}

impl ShredSigVerifyStats {
    const METRICS_SUBMIT_CADENCE: Duration = Duration::from_secs(2);

    fn new(now: Instant) -> Self {
        Self {
            since: now,
            num_iters: 0usize,
            num_batches: 0usize,
            num_packets: 0usize,
            num_discards_pre: 0usize,
            num_deduper_saturations: 0usize,
            num_discards_post: 0usize,
            num_duplicates: 0usize,
            num_invalid_retransmitter: AtomicUsize::default(),
            num_retranmitter_signature_skipped: AtomicUsize::default(),
            num_retranmitter_signature_verified: AtomicUsize::default(),
            num_retransmit_stage_overflow_shreds: 0usize,
            num_retransmit_shreds: 0usize,
            num_unknown_block_location: 0usize,
            num_unknown_slot_leader: AtomicUsize::default(),
            num_unknown_turbine_parent: AtomicUsize::default(),
            elapsed_micros: 0u64,
            resign_micros: 0u64,
        }
    }

    fn maybe_submit(&mut self) {
        if self.since.elapsed() <= Self::METRICS_SUBMIT_CADENCE {
            return;
        }
        datapoint_info!(
            "shred_sigverify",
            ("num_iters", self.num_iters, i64),
            ("num_batches", self.num_batches, i64),
            ("num_packets", self.num_packets, i64),
            ("num_discards_pre", self.num_discards_pre, i64),
            ("num_deduper_saturations", self.num_deduper_saturations, i64),
            ("num_discards_post", self.num_discards_post, i64),
            ("num_duplicates", self.num_duplicates, i64),
            (
                "num_invalid_retransmitter",
                self.num_invalid_retransmitter.load(Ordering::Relaxed),
                i64
            ),
            (
                "num_retranmitter_signature_skipped",
                self.num_retranmitter_signature_skipped
                    .load(Ordering::Relaxed),
                i64
            ),
            (
                "num_retranmitter_signature_verified",
                self.num_retranmitter_signature_verified
                    .load(Ordering::Relaxed),
                i64
            ),
            (
                "num_retransmit_stage_overflow_shreds",
                self.num_retransmit_stage_overflow_shreds,
                i64
            ),
            ("num_retransmit_shreds", self.num_retransmit_shreds, i64),
            (
                "num_unknown_block_location",
                self.num_unknown_block_location,
                i64
            ),
            (
                "num_unknown_slot_leader",
                self.num_unknown_slot_leader.load(Ordering::Relaxed),
                i64
            ),
            (
                "num_unknown_turbine_parent",
                self.num_unknown_turbine_parent.load(Ordering::Relaxed),
                i64
            ),
            ("elapsed_micros", self.elapsed_micros, i64),
            ("resign_micros", self.resign_micros, i64),
        );
        *self = Self::new(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        rand::Rng,
        solana_entry::entry::{Entry, create_ticks},
        solana_gossip::contact_info::ContactInfo,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_ledger::{
            genesis_utils::create_genesis_config_with_leader,
            shred::{Nonce, ProcessShredsStats, ReedSolomonCache, Shredder},
        },
        solana_net_utils::SocketAddrSpace,
        solana_perf::packet::{Packet, PacketFlags, RecycledPacketBatch},
        solana_runtime::bank::Bank,
        solana_signer::Signer,
        solana_time_utils::timestamp,
        test_case::test_matrix,
    };

    #[test]
    fn test_verify_packet_signature() {
        let leader_keypair = Arc::new(Keypair::new());
        let wrong_keypair = Keypair::new();
        let leader_pubkey = leader_keypair.pubkey();
        let bank = Bank::new_for_tests(
            &create_genesis_config_with_leader(100, &leader_pubkey, 10).genesis_config,
        );
        let leader_schedule_cache = LeaderScheduleCache::new_from_bank(&bank);
        let bank_forks = BankForks::new_rw_arc(bank);
        let batch_size = 2;
        let mut batch = RecycledPacketBatch::with_capacity(batch_size);
        batch.resize(batch_size, Packet::default());
        let mut batches = vec![batch];

        let entries = create_ticks(1, 1, Hash::new_unique());
        let shredder = Shredder::new(1, 0, 1, 0).unwrap();
        let (shreds_data, _shreds_code) = shredder.entries_to_merkle_shreds_for_tests(
            &leader_keypair,
            &entries,
            true,
            Hash::new_unique(),
            0,
            0,
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        );
        let (shreds_data_wrong, _shreds_code_wrong) = shredder.entries_to_merkle_shreds_for_tests(
            &wrong_keypair,
            &entries,
            true,
            Hash::new_unique(),
            0,
            0,
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        );

        let shred = shreds_data[0].clone();
        batches[0][0].buffer_mut()[..shred.payload().len()].copy_from_slice(shred.payload());
        batches[0][0].meta_mut().size = shred.payload().len();

        let shred = shreds_data_wrong[0].clone();
        batches[0][1].buffer_mut()[..shred.payload().len()].copy_from_slice(shred.payload());
        batches[0][1].meta_mut().size = shred.payload().len();

        let working_bank = bank_forks.read().unwrap().working_bank();
        let batches = batches
            .into_iter()
            .map(PacketBatch::from)
            .collect::<Vec<_>>();
        assert!(verify_packet_signature(
            &Pubkey::new_unique(), // self_pubkey
            batches[0].get(0).unwrap(),
            &working_bank,
            &leader_schedule_cache,
        ));
        assert!(!verify_packet_signature(
            &Pubkey::new_unique(), // self_pubkey
            batches[0].get(1).unwrap(),
            &working_bank,
            &leader_schedule_cache,
        ));
    }

    #[test_matrix(
        [true, false],
        [true, false]
    )]
    fn test_maybe_verify_and_resign_packet(repaired: bool, is_last_in_slot: bool) {
        let mut rng = rand::rng();

        let leader_keypair = Arc::new(Keypair::new());
        let leader_pubkey = leader_keypair.pubkey();
        let bank = Bank::new_for_tests(
            &create_genesis_config_with_leader(100, &leader_pubkey, 10).genesis_config,
        );
        let leader_schedule_cache = LeaderScheduleCache::new_from_bank(&bank);
        let bank_forks = BankForks::new_rw_arc(bank);
        let (working_bank, root_bank) = {
            let bank_forks = bank_forks.read().unwrap();
            (bank_forks.working_bank(), bank_forks.root_bank())
        };
        let chained_merkle_root = Hash::new_from_array(rng.random());

        let shredder = Shredder::new(root_bank.slot(), root_bank.parent_slot(), 0, 0).unwrap();
        let entries = vec![Entry::new(&Hash::default(), 0, vec![])];
        let mut shreds = shredder.make_merkle_shreds_from_entries(
            &leader_keypair,
            &entries,
            is_last_in_slot,
            chained_merkle_root,
            0,
            0,
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        );

        let cluster_info = ClusterInfo::new(
            ContactInfo::new_localhost(&leader_pubkey, timestamp()),
            leader_keypair,
            SocketAddrSpace::Unspecified,
        );

        let cluster_nodes_cache = ClusterNodesCache::<RetransmitStage>::new(
            CLUSTER_NODES_CACHE_NUM_EPOCH_CAP,
            CLUSTER_NODES_CACHE_TTL,
        );
        let stats = ShredSigVerifyStats::new(Instant::now());

        for shred in shreds.iter_mut() {
            let keypair = Keypair::new();
            let nonce = repaired.then(|| rng.random::<Nonce>());
            if is_last_in_slot {
                let packet = &mut shred.payload().to_packet(nonce);
                let buf_before = packet.buffer_mut().to_vec();
                if repaired {
                    packet.meta_mut().flags |= PacketFlags::REPAIR;
                }
                maybe_verify_and_resign_packet(
                    &mut packet.into(),
                    &root_bank,
                    &working_bank,
                    &cluster_info,
                    &leader_schedule_cache,
                    &cluster_nodes_cache,
                    &stats,
                    &keypair,
                )
                .expect("packet should pass the verification");
                assert!(!packet.meta().discard());

                // Check whether the packet was modified.
                assert_ne!(&buf_before, &packet.data(..).unwrap());

                let mut bytes_packet = shred.payload().to_bytes_packet(nonce);
                if repaired {
                    bytes_packet.meta_mut().flags |= PacketFlags::REPAIR;
                }
                let buf_addr = bytes_packet.buffer().as_ptr().addr();
                maybe_verify_and_resign_packet(
                    &mut bytes_packet.as_mut(),
                    &root_bank,
                    &working_bank,
                    &cluster_info,
                    &leader_schedule_cache,
                    &cluster_nodes_cache,
                    &stats,
                    &keypair,
                )
                .expect("packet should pass the verification");
                assert!(!bytes_packet.meta().discard());

                // Check whether the packet was modified.
                let buf_addr_after = bytes_packet.buffer().as_ptr().addr();
                assert_ne!(buf_addr, buf_addr_after);
            } else {
                let packet = &mut shred.payload().to_packet(nonce);
                if repaired {
                    packet.meta_mut().flags |= PacketFlags::REPAIR;
                }
                maybe_verify_and_resign_packet(
                    &mut packet.into(),
                    &root_bank,
                    &working_bank,
                    &cluster_info,
                    &leader_schedule_cache,
                    &cluster_nodes_cache,
                    &stats,
                    &keypair,
                )
                .expect("packet should pass the verification");
                assert!(!packet.meta().discard());

                let mut bytes_packet = shred.payload().to_bytes_packet(nonce);
                if repaired {
                    bytes_packet.meta_mut().flags |= PacketFlags::REPAIR;
                }
                let buf_addr = bytes_packet.buffer().as_ptr().addr();
                maybe_verify_and_resign_packet(
                    &mut bytes_packet.as_mut(),
                    &root_bank,
                    &working_bank,
                    &cluster_info,
                    &leader_schedule_cache,
                    &cluster_nodes_cache,
                    &stats,
                    &keypair,
                )
                .expect("packet should pass the verification");
                assert!(!packet.meta().discard());

                // Packet should not be modified.
                let buf_addr_after = bytes_packet.buffer().as_ptr().addr();
                assert_eq!(buf_addr, buf_addr_after);
            }
        }
    }
}
