use {
    crate::{
        cluster_nodes::{ClusterNodesCache, DATA_PLANE_FANOUT},
        retransmit_stage::RetransmitStage,
    },
    bytes::Bytes,
    crossbeam_channel::{Receiver, RecvTimeoutError, SendError, Sender},
    itertools::{Either, Itertools},
    rayon::{ThreadPool, ThreadPoolBuilder, prelude::*},
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_keypair::Keypair,
    solana_ledger::{
        blockstore_meta::BlockLocation,
        leader_schedule_cache::LeaderScheduleCache,
        shred::{
            Admissible, AnyShred, Nonce, Shred, filter::ShredFilterContext, parse_repair,
            parse_turbine,
        },
        sigverify_shreds::{LruCache, SlotPubkeys, verify_shred},
    },
    solana_perf::{deduper::Deduper, packet::PacketBatch},
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    solana_signer::Signer,
    solana_streamer::{evicting_sender::EvictingSender, streamer::ChannelSend},
    std::{
        collections::HashSet,
        num::NonZeroUsize,
        sync::{
            Arc, RwLock,
            atomic::{AtomicUsize, Ordering},
        },
        thread::{Builder, JoinHandle},
        time::{Duration, Instant},
    },
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

pub type RepairNonceLocationLookup = dyn Fn(Nonce) -> Option<BlockLocation> + Send + Sync;

pub fn spawn_shred_sigverify(
    cluster_info: Arc<ClusterInfo>,
    bank_forks: Arc<RwLock<BankForks>>,
    leader_schedule_cache: Arc<LeaderScheduleCache>,
    shred_fetch_receiver: Receiver<PacketBatch>,
    retransmit_sender: EvictingSender<Vec<Shred>>,
    verified_sender: Sender<Vec<(Shred, /*is_repaired:*/ bool, BlockLocation)>>,
    repair_nonce_location_lookup: Arc<RepairNonceLocationLookup>,
    num_sigverify_threads: NonZeroUsize,
) -> JoinHandle<()> {
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
        let mut rng = rand::rng();
        let deduper = Deduper::<2, [u8]>::new(&mut rng, DEDUPER_NUM_BITS);
        let mut shred_buffer = Vec::with_capacity(SIGVERIFY_SHRED_BATCH_SIZE);
        let mut filter_ctx = {
            let root_bank = bank_forks.read().unwrap().root_bank();
            ShredFilterContext::new(root_bank, cluster_info.my_shred_version())
        };
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
                &mut filter_ctx,
                &shred_fetch_receiver,
                &retransmit_sender,
                &verified_sender,
                &cluster_nodes_cache,
                repair_nonce_location_lookup.as_ref(),
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
    deduper: &Deduper<K, [u8]>,
    filter_ctx: &mut ShredFilterContext,
    shred_fetch_receiver: &Receiver<PacketBatch>,
    retransmit_sender: &EvictingSender<Vec<Shred>>,
    verified_sender: &Sender<Vec<(Shred, /*is_repaired:*/ bool, BlockLocation)>>,
    cluster_nodes_cache: &ClusterNodesCache<RetransmitStage>,
    repair_nonce_location_lookup: &RepairNonceLocationLookup,
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
    let (working_bank, root_bank) = {
        let bank_forks = bank_forks.read().unwrap();
        (bank_forks.working_bank(), bank_forks.root_bank())
    };
    filter_ctx.maybe_update(root_bank.clone());
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
    let shreds = thread_pool.install(|| parse_packets(shred_buffer, deduper, filter_ctx, stats));
    let self_pubkey = keypair.pubkey();
    let slot_leaders =
        get_slot_leaders(&self_pubkey, &shreds, leader_schedule_cache, &working_bank);
    let num_admissible = shreds.len();
    let shreds: Vec<(Shred, Option<Nonce>)> = thread_pool.install(|| {
        shreds
            .into_par_iter()
            .filter_map(|(shred, nonce)| {
                let shred = verify_shred(shred, &slot_leaders, cache)?;
                Some((shred, nonce))
            })
            .collect()
    });
    stats.num_discards_post += num_admissible - shreds.len();
    // Verify retransmitter's signature, and resign shreds
    // Merkle root as the retransmitter node.
    let resign_start = Instant::now();
    let shreds: Vec<(Shred, Option<Nonce>)> = thread_pool.install(|| {
        shreds
            .into_par_iter()
            .filter_map(|(shred, nonce)| {
                let shred = maybe_verify_and_resign_shred(
                    shred,
                    nonce.is_some(),
                    &root_bank,
                    &working_bank,
                    cluster_info,
                    leader_schedule_cache,
                    cluster_nodes_cache,
                    stats,
                    keypair,
                )?;
                Some((shred, nonce))
            })
            .collect()
    });
    stats.resign_micros += resign_start.elapsed().as_micros() as u64;
    let (shreds, repairs): (Vec<Shred>, Vec<_>) = shreds
        .into_iter()
        .filter_map(|(shred, nonce)| {
            let Some(nonce) = nonce else {
                return Some(Either::Left(shred));
            };
            if let Some(location) = repair_nonce_location_lookup(nonce) {
                Some(Either::Right((shred, /*is_repaired:*/ true, location)))
            } else {
                stats.num_unknown_block_location += 1;
                None
            }
        })
        .partition_map(|either| either);

    stats.num_retransmit_shreds += shreds.len();
    if let Err(send_err) = retransmit_sender.try_send(shreds.clone()) {
        match send_err {
            crossbeam_channel::TrySendError::Full(v) => {
                stats.num_retransmit_stage_overflow_shreds += v.len();
            }
            _ => unreachable!("EvictingSender holds on to both ends of the channel"),
        }
    }
    let shreds = shreds
        .into_iter()
        .map(|shred| (shred, /*is_repaired:*/ false, BlockLocation::Original));
    verified_sender.send(shreds.chain(repairs).collect())?;
    stats.elapsed_micros += now.elapsed().as_micros() as u64;
    shred_buffer.clear();
    Ok(())
}

fn parse_packets<const K: usize>(
    packets: &[PacketBatch],
    deduper: &Deduper<K, [u8]>,
    filter_ctx: &ShredFilterContext,
    stats: &ShredSigVerifyStats,
) -> Vec<(AnyShred<Admissible>, Option<Nonce>)> {
    packets
        .par_iter()
        .flat_map_iter(|batch| batch.iter())
        .filter(|packet| !packet.meta().discard())
        .filter_map(|packet| {
            let bytes = Bytes::copy_from_slice(packet.data(..)?);
            let parsed = if packet.meta().repair() {
                parse_repair(bytes).map(|(shred, nonce)| (shred, Some(nonce)))
            } else {
                parse_turbine(bytes).map(|shred| (shred, None))
            };
            let Ok((shred, nonce)) = parsed else {
                stats.num_parse_failed.fetch_add(1, Ordering::Relaxed);
                return None;
            };
            let policy = filter_ctx.policy(shred.slot());
            let Ok(shred) = shred.check_policy(&policy) else {
                stats.num_policy_rejected.fetch_add(1, Ordering::Relaxed);
                return None;
            };
            if deduper.dedup(shred.bytes()) && nonce.is_none() {
                stats.num_duplicates.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            Some((shred, nonce))
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn maybe_verify_and_resign_shred(
    shred: Shred,
    is_repair: bool,
    root_bank: &Bank,
    working_bank: &Bank,
    cluster_info: &ClusterInfo,
    leader_schedule_cache: &LeaderScheduleCache,
    cluster_nodes_cache: &ClusterNodesCache<RetransmitStage>,
    stats: &ShredSigVerifyStats,
    keypair: &Keypair,
) -> Option<Shred> {
    // Repair packets do not follow turbine tree and
    // are verified using the trailing nonce.
    if !is_repair
        && shred.retransmitter_signature().is_some()
        && !verify_retransmitter_signature(
            &shred,
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
    }
    match shred.resign(keypair) {
        Ok(shred) => Some(shred),
        Err(err) => {
            error!("resign: {err:?}");
            stats.num_resign_failed.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

#[must_use]
fn verify_retransmitter_signature(
    shred: &Shred,
    root_bank: &Bank,
    working_bank: &Bank,
    cluster_info: &ClusterInfo,
    leader_schedule_cache: &LeaderScheduleCache,
    cluster_nodes_cache: &ClusterNodesCache<RetransmitStage>,
    stats: &ShredSigVerifyStats,
) -> bool {
    let Some(leader) = leader_schedule_cache.slot_leader_at(shred.slot(), Some(working_bank))
    else {
        stats
            .num_unknown_slot_leader
            .fetch_add(1, Ordering::Relaxed);
        return false;
    };
    let cluster_nodes =
        cluster_nodes_cache.get(shred.slot(), root_bank, working_bank, cluster_info);
    let parent =
        match cluster_nodes.get_retransmit_parent(&leader.id, &shred.id(), DATA_PLANE_FANOUT) {
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
    if shred.verify_retransmitter(&parent).is_ok() {
        stats
            .num_retranmitter_signature_verified
            .fetch_add(1, Ordering::Relaxed);
        true
    } else {
        false
    }
}

// Returns pubkey of leaders for shred slots referenced in the shreds.
// Slots are left out if:
//   - slot leader is unknown.
//   - slot leader is the node itself (circular transmission).
fn get_slot_leaders(
    self_pubkey: &Pubkey,
    shreds: &[(AnyShred<Admissible>, Option<Nonce>)],
    leader_schedule_cache: &LeaderScheduleCache,
    bank: &Bank,
) -> SlotPubkeys {
    shreds
        .iter()
        .map(|(shred, _)| shred.slot())
        .collect::<HashSet<Slot>>()
        .into_iter()
        .filter_map(|slot| {
            let leader = leader_schedule_cache
                .slot_leader_at(slot, Some(bank))
                .map(|leader| leader.id)
                .filter(|leader| leader != self_pubkey)?;
            Some((slot, leader))
        })
        .collect()
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
    num_duplicates: AtomicUsize,
    num_parse_failed: AtomicUsize,
    num_policy_rejected: AtomicUsize,
    num_invalid_retransmitter: AtomicUsize,
    num_resign_failed: AtomicUsize,
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
            num_duplicates: AtomicUsize::default(),
            num_parse_failed: AtomicUsize::default(),
            num_policy_rejected: AtomicUsize::default(),
            num_invalid_retransmitter: AtomicUsize::default(),
            num_resign_failed: AtomicUsize::default(),
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
            (
                "num_duplicates",
                self.num_duplicates.load(Ordering::Relaxed),
                i64
            ),
            (
                "num_parse_failed",
                self.num_parse_failed.load(Ordering::Relaxed),
                i64
            ),
            (
                "num_policy_rejected",
                self.num_policy_rejected.load(Ordering::Relaxed),
                i64
            ),
            (
                "num_invalid_retransmitter",
                self.num_invalid_retransmitter.load(Ordering::Relaxed),
                i64
            ),
            (
                "num_resign_failed",
                self.num_resign_failed.load(Ordering::Relaxed),
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
            shred::{ProcessShredsStats, Shredder},
        },
        solana_net_utils::SocketAddrSpace,
        solana_perf::packet::{Packet, PacketFlags, RecycledPacketBatch},
        solana_runtime::bank::Bank,
        solana_signer::Signer,
        solana_time_utils::timestamp,
        test_case::test_matrix,
    };

    fn to_packet(shred: &Shred, nonce: Option<Nonce>) -> Packet {
        let mut packet = Packet::default();
        let bytes = shred.bytes();
        packet.buffer_mut()[..bytes.len()].copy_from_slice(bytes);
        let mut size = bytes.len();
        if let Some(nonce) = nonce {
            packet.buffer_mut()[size..][..4].copy_from_slice(&nonce.to_le_bytes());
            size += 4;
            packet.meta_mut().flags |= PacketFlags::REPAIR;
        }
        packet.meta_mut().size = size;
        packet
    }

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
        let (working_bank, root_bank) = {
            let bank_forks = bank_forks.read().unwrap();
            (bank_forks.working_bank(), bank_forks.root_bank())
        };

        let entries = create_ticks(1, 1, Hash::new_unique());
        let shredder = Shredder::new(1, 0, 1, 0).unwrap();
        let (shreds_data, _shreds_code) = shredder.entries_to_merkle_shreds_for_tests(
            &leader_keypair,
            &entries,
            true,
            Hash::new_unique(),
            0,
            &mut ProcessShredsStats::default(),
        );
        let (shreds_data_wrong, _shreds_code_wrong) = shredder.entries_to_merkle_shreds_for_tests(
            &wrong_keypair,
            &entries,
            true,
            Hash::new_unique(),
            0,
            &mut ProcessShredsStats::default(),
        );

        let mut batch = RecycledPacketBatch::with_capacity(2);
        batch.push(to_packet(&shreds_data[0], None));
        batch.push(to_packet(&shreds_data_wrong[0], None));
        let batches = vec![PacketBatch::from(batch)];

        let mut rng = rand::rng();
        let deduper = Deduper::<2, [u8]>::new(&mut rng, /*num_bits:*/ 640_007);
        let filter_ctx = ShredFilterContext::new(root_bank, /*shred_version:*/ 0);
        let stats = ShredSigVerifyStats::new(Instant::now());
        let shreds = parse_packets(&batches, &deduper, &filter_ctx, &stats);
        assert_eq!(shreds.len(), 2);

        let cache = RwLock::new(LruCache::new(/*capacity:*/ 128));
        let slot_leaders = get_slot_leaders(
            &Pubkey::new_unique(), // self_pubkey
            &shreds,
            &leader_schedule_cache,
            &working_bank,
        );
        let verified: Vec<_> = shreds
            .into_iter()
            .map(|(shred, _)| verify_shred(shred, &slot_leaders, &cache))
            .collect();
        assert!(verified[0].is_some());
        assert!(verified[1].is_none());

        let shreds = parse_packets(&batches, &deduper, &filter_ctx, &stats);
        assert!(shreds.is_empty());
        assert_eq!(stats.num_duplicates.load(Ordering::Relaxed), 2);
    }

    #[test_matrix(
        [true, false],
        [true, false]
    )]
    fn test_maybe_verify_and_resign_shred(repaired: bool, is_last_in_slot: bool) {
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

        let slot = root_bank.slot() + 1;
        let shredder = Shredder::new(slot, root_bank.slot(), 0, 0).unwrap();
        let entries = vec![Entry::new(&Hash::default(), 0, vec![])];
        let shreds = shredder.make_merkle_shreds_from_entries(
            &leader_keypair,
            &entries,
            is_last_in_slot,
            chained_merkle_root,
            0,
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
        let deduper = Deduper::<2, [u8]>::new(&mut rng, /*num_bits:*/ 640_007);
        let filter_ctx = ShredFilterContext::new(root_bank.clone(), /*shred_version:*/ 0);

        for shred in shreds {
            let keypair = Keypair::new();
            let nonce = repaired.then(|| rng.random::<Nonce>());
            let mut batch = RecycledPacketBatch::with_capacity(1);
            batch.push(to_packet(&shred, nonce));
            let batches = vec![PacketBatch::from(batch)];
            let mut parsed = parse_packets(&batches, &deduper, &filter_ctx, &stats);
            let (received, parsed_nonce) = parsed.pop().expect("packet should parse");
            assert_eq!(parsed_nonce, nonce);
            let received = received
                .verify(&leader_pubkey)
                .expect("leader signature should verify");
            assert_eq!(received.bytes(), shred.bytes());

            let resigned = maybe_verify_and_resign_shred(
                received,
                repaired,
                &root_bank,
                &working_bank,
                &cluster_info,
                &leader_schedule_cache,
                &cluster_nodes_cache,
                &stats,
                &keypair,
            )
            .expect("shred should pass the verification");

            if is_last_in_slot {
                assert_ne!(resigned.bytes(), shred.bytes());
                assert_eq!(
                    resigned.retransmitter_signature(),
                    Some(&keypair.sign_message(resigned.merkle_root().unwrap().as_ref()))
                );
            } else {
                assert_eq!(resigned.bytes(), shred.bytes());
                assert!(resigned.retransmitter_signature().is_none());
            }
        }
        assert_eq!(stats.num_resign_failed.load(Ordering::Relaxed), 0);
    }
}
