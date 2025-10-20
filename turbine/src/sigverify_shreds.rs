use {
    crate::{
        cluster_nodes::{self, check_feature_activation, ClusterNodesCache},
        retransmit_stage::RetransmitStage,
    },
    agave_feature_set as feature_set,
    crossbeam_channel::{Receiver, RecvTimeoutError, SendError, Sender},
    itertools::{Either, Itertools},
    rayon::{prelude::*, ThreadPool, ThreadPoolBuilder},
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_keypair::Keypair,
    solana_ledger::{
        leader_schedule_cache::LeaderScheduleCache,
        shred::{
            self,
            layout::{get_shred, resign_packet},
            wire::is_retransmitter_signed_variant,
        },
        sigverify_shreds::{verify_shreds_gpu, LruCache, SlotPubkeys},
    },
    solana_perf::{
        self,
        deduper::Deduper,
        packet::{PacketBatch, PacketRefMut},
        recycler_cache::RecyclerCache,
    },
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    solana_signer::Signer,
    solana_streamer::{evicting_sender::EvictingSender, streamer::ChannelSend},
    std::{
        num::NonZeroUsize,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, RwLock,
        },
        thread::{Builder, JoinHandle},
        time::{Duration, Instant},
    },
    thiserror::Error,
};

// 34MB where each cache entry is 136 bytes.
const SIGVERIFY_LRU_CACHE_CAPACITY: usize = 1 << 18;

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

pub fn spawn_shred_sigverify(
    cluster_info: Arc<ClusterInfo>,
    bank_forks: Arc<RwLock<BankForks>>,
    leader_schedule_cache: Arc<LeaderScheduleCache>,
    shred_fetch_receiver: Receiver<PacketBatch>,
    retransmit_sender: EvictingSender<Vec<shred::Payload>>,
    verified_sender: Sender<Vec<(shred::Payload, /*is_repaired:*/ bool)>>,
    num_sigverify_threads: NonZeroUsize,
) -> JoinHandle<()> {
    let recycler_cache = RecyclerCache::warmed();
    let mut stats = ShredSigVerifyStats::new(Instant::now());
    let cache = RwLock::new(LruCache::new(SIGVERIFY_LRU_CACHE_CAPACITY));
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
        let mut rng = rand::thread_rng();
        let mut deduper = Deduper::<2, [u8]>::new(&mut rng, DEDUPER_NUM_BITS);
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
                &recycler_cache,
                &deduper,
                &shred_fetch_receiver,
                &retransmit_sender,
                &verified_sender,
                &cluster_nodes_cache,
                &cache,
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
    recycler_cache: &RecyclerCache,
    deduper: &Deduper<K, [u8]>,
    shred_fetch_receiver: &Receiver<PacketBatch>,
    retransmit_sender: &EvictingSender<Vec<shred::Payload>>,
    verified_sender: &Sender<Vec<(shred::Payload, /*is_repaired:*/ bool)>>,
    cluster_nodes_cache: &ClusterNodesCache<RetransmitStage>,
    cache: &RwLock<LruCache>,
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
    stats.num_duplicates += thread_pool.install(|| {
        shred_buffer
            .par_iter_mut()
            .flatten()
            .filter(|packet| {
                !packet.meta().discard()
                    && shred::wire::get_shred(packet.as_ref())
                        .map(|shred| deduper.dedup(shred))
                        .unwrap_or(true)
                    && !packet.meta().repair()
            })
            .map(|mut packet| packet.meta_mut().set_discard(true))
            .count()
    });
    let (working_bank, root_bank) = {
        let bank_forks = bank_forks.read().unwrap();
        (bank_forks.working_bank(), bank_forks.root_bank())
    };
    verify_packets(
        thread_pool,
        &keypair.pubkey(),
        &working_bank,
        leader_schedule_cache,
        recycler_cache,
        shred_buffer,
        cache,
    );
    stats.num_discards_post += count_discards(shred_buffer);
    // Verify retransmitter's signature, and resign shreds
    // Merkle root as the retransmitter node.
    let resign_start = Instant::now();
    thread_pool.install(|| {
        shred_buffer
            .par_iter_mut()
            .flatten()
            .filter(|packet| !packet.meta().discard())
            .for_each(|mut packet| {
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
            })
    });
    stats.resign_micros += resign_start.elapsed().as_micros() as u64;
    // Extract shred payload from packets, and separate out repaired shreds.
    let (shreds, repairs): (Vec<_>, Vec<_>) = shred_buffer
        .iter()
        .flat_map(|batch| batch.iter())
        .filter(|packet| !packet.meta().discard())
        .filter_map(|packet| {
            let shred = shred::layout::get_shred(packet)?.to_vec();
            Some((shred, packet.meta().repair()))
        })
        .partition_map(|(shred, repair)| {
            if repair {
                // No need for Arc overhead here because repaired shreds are
                // not retranmitted.
                Either::Right(shred::Payload::from(shred))
            } else {
                // Share the payload between the retransmit-stage and the
                // window-service.
                Either::Left(shred::Payload::from(shred))
            }
        });
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
        .map(|shred| (shred, /*is_repaired:*/ false));
    let repairs = repairs
        .into_iter()
        .map(|shred| (shred, /*is_repaired:*/ true));
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
                    check_feature_activation(
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
    let data_plane_fanout = cluster_nodes::get_data_plane_fanout(shred.slot(), root_bank);
    let parent = match cluster_nodes.get_retransmit_parent(&leader, &shred, data_plane_fanout) {
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

fn verify_packets(
    thread_pool: &ThreadPool,
    self_pubkey: &Pubkey,
    working_bank: &Bank,
    leader_schedule_cache: &LeaderScheduleCache,
    recycler_cache: &RecyclerCache,
    packets: &mut [PacketBatch],
    cache: &RwLock<LruCache>,
) {
    let leader_slots: SlotPubkeys =
        get_slot_leaders(self_pubkey, packets, leader_schedule_cache, working_bank)
            .filter_map(|(slot, pubkey)| Some((slot, pubkey?)))
            .chain(std::iter::once((Slot::MAX, Pubkey::default())))
            .collect();
    let out = verify_shreds_gpu(thread_pool, packets, &leader_slots, recycler_cache, cache);
    solana_perf::sigverify::mark_disabled(packets, &out);
}

// Returns pubkey of leaders for shred slots referenced in the packets.
// Marks packets as discard if:
//   - fails to deserialize the shred slot.
//   - slot leader is unknown.
//   - slot leader is the node itself (circular transmission).
fn get_slot_leaders<'a>(
    self_pubkey: &'a Pubkey,
    batches: &'a mut [PacketBatch],
    leader_schedule_cache: &'a LeaderScheduleCache,
    bank: &'a Bank,
) -> impl Iterator<Item = (Slot, Option<Pubkey>)> + 'a {
    batches
        .iter_mut()
        .flat_map(|batch| batch.iter_mut())
        .filter(|packet| !packet.meta().discard())
        .filter_map(move |mut packet| {
            let shred = shred::layout::get_shred(packet.as_ref());
            let slot = shred.and_then(shred::layout::get_slot)?;
            let leader = leader_schedule_cache
                .slot_leader_at(slot, Some(bank))
                .filter(|leader| leader != self_pubkey);
            if leader.is_none() {
                packet.meta_mut().set_discard(true);
            }
            Some((slot, leader))
        })
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
        solana_entry::entry::{create_ticks, Entry},
        solana_gossip::contact_info::ContactInfo,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_ledger::{
            genesis_utils::create_genesis_config_with_leader,
            shred::{Nonce, ProcessShredsStats, ReedSolomonCache, Shredder},
        },
        solana_perf::packet::{Packet, PacketFlags, PinnedPacketBatch},
        solana_runtime::bank::Bank,
        solana_signer::Signer,
        solana_streamer::socket::SocketAddrSpace,
        solana_time_utils::timestamp,
        test_case::test_matrix,
    };

    #[test]
    fn test_sigverify_shreds_verify_batches() {
        let leader_keypair = Arc::new(Keypair::new());
        let wrong_keypair = Keypair::new();
        let leader_pubkey = leader_keypair.pubkey();
        let bank = Bank::new_for_tests(
            &create_genesis_config_with_leader(100, &leader_pubkey, 10).genesis_config,
        );
        let leader_schedule_cache = LeaderScheduleCache::new_from_bank(&bank);
        let bank_forks = BankForks::new_rw_arc(bank);
        let batch_size = 2;
        let mut batch = PinnedPacketBatch::with_capacity(batch_size);
        batch.resize(batch_size, Packet::default());
        let mut batches = vec![batch];

        let entries = create_ticks(1, 1, Hash::new_unique());
        let shredder = Shredder::new(1, 0, 1, 0).unwrap();
        let (shreds_data, _shreds_code) = shredder.entries_to_merkle_shreds_for_tests(
            &leader_keypair,
            &entries,
            true,
            Some(Hash::new_unique()),
            0,
            0,
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        );
        let (shreds_data_wrong, _shreds_code_wrong) = shredder.entries_to_merkle_shreds_for_tests(
            &wrong_keypair,
            &entries,
            true,
            Some(Hash::new_unique()),
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

        let cache = RwLock::new(LruCache::new(/*capacity:*/ 128));
        let thread_pool = ThreadPoolBuilder::new().num_threads(3).build().unwrap();
        let working_bank = bank_forks.read().unwrap().working_bank();
        let mut batches = batches
            .into_iter()
            .map(PacketBatch::from)
            .collect::<Vec<_>>();
        verify_packets(
            &thread_pool,
            &Pubkey::new_unique(), // self_pubkey
            &working_bank,
            &leader_schedule_cache,
            &RecyclerCache::warmed(),
            &mut batches,
            &cache,
        );
        assert!(!batches[0].get(0).unwrap().meta().discard());
        assert!(batches[0].get(1).unwrap().meta().discard());
    }

    #[test_matrix(
        [true, false],
        [true, false]
    )]
    fn test_maybe_verify_and_resign_packet(repaired: bool, is_last_in_slot: bool) {
        let mut rng = rand::thread_rng();

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

        let chained_merkle_root = Some(Hash::new_from_array(rng.gen()));

        let shredder = Shredder::new(root_bank.slot(), root_bank.parent_slot(), 0, 0).unwrap();
        let entries = vec![Entry::new(&Hash::default(), 0, vec![])];
        let mut shreds: Vec<_> = shredder
            .make_merkle_shreds_from_entries(
                &leader_keypair,
                &entries,
                is_last_in_slot,
                chained_merkle_root,
                0,
                0,
                &ReedSolomonCache::default(),
                &mut ProcessShredsStats::default(),
            )
            .collect();

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
            let nonce = repaired.then(|| rng.gen::<Nonce>());
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
