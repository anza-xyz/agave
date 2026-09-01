use {
    crate::{
        cluster_nodes::{ClusterNodesCache, DATA_PLANE_FANOUT},
        retransmit_stage::RetransmitStage,
    },
    agave_feature_set as feature_set,
    crossbeam_channel::{Receiver, RecvTimeoutError, SendError, Sender, bounded},
    scopeguard::defer,
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_keypair::Keypair,
    solana_ledger::{
        blockstore_meta::BlockLocation,
        leader_schedule_cache::LeaderScheduleCache,
        shred::{
            self,
            layout::get_shred,
            wire::{is_retransmitter_signed_variant, resign_packet},
        },
        sigverify_shreds::{LruCache, SlotPubkeys, verify_shred_cpu},
    },
    solana_perf::{
        deduper::Deduper,
        packet::{PacketBatch, PacketRef, PacketRefMut},
    },
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::Bank,
        bank_forks::{BankForks, SharableBanks},
    },
    solana_signer::Signer,
    solana_streamer::{evicting_sender::EvictingSender, streamer::ChannelSend},
    std::{
        num::NonZeroUsize,
        sync::{Arc, RwLock},
        thread::{self, Builder, JoinHandle},
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

pub type RepairNonceLocationLookup = dyn Fn(shred::Nonce) -> Option<BlockLocation> + Send + Sync;

#[derive(Default)]
struct WorkerStats {
    num_discards_post: usize,
    num_duplicates: usize,
    num_invalid_retransmitter: usize,
    num_retranmitter_signature_skipped: usize,
    num_retranmitter_signature_verified: usize,
    num_unknown_slot_leader: usize,
    num_unknown_turbine_parent: usize,
}
impl WorkerStats {
    fn add_batch(&mut self, counters: WorkerCounters) {
        self.num_discards_post += usize::from(counters.num_discards_post);
        self.num_duplicates += usize::from(counters.num_duplicates);
        self.num_invalid_retransmitter += usize::from(counters.num_invalid_retransmitter);
        self.num_retranmitter_signature_skipped +=
            usize::from(counters.num_retranmitter_signature_skipped);
        self.num_retranmitter_signature_verified +=
            usize::from(counters.num_retranmitter_signature_verified);
        self.num_unknown_slot_leader += usize::from(counters.num_unknown_slot_leader);
        self.num_unknown_turbine_parent += usize::from(counters.num_unknown_turbine_parent);
    }
}
#[derive(Default)]
struct WorkerCounters {
    num_discards_post: u16,
    num_duplicates: u16,
    num_invalid_retransmitter: u16,
    num_retranmitter_signature_skipped: u16,
    num_retranmitter_signature_verified: u16,
    num_unknown_slot_leader: u16,
    num_unknown_turbine_parent: u16,
}

#[derive(Clone)]
struct WorkerContext {
    cache: Arc<RwLock<LruCache>>,
    cluster_info: Arc<ClusterInfo>,
    sharable_banks: SharableBanks,
    leader_schedule_cache: Arc<LeaderScheduleCache>,
    cluster_nodes_cache: Arc<ClusterNodesCache<RetransmitStage>>,
    deduper: Arc<Deduper<2, [u8]>>,
}

fn verify_batch(batch: &mut PacketBatch, ctx: &WorkerContext, keypair: &Keypair) -> WorkerCounters {
    let mut counters = WorkerCounters::default();

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
    for mut packet in batch.iter_mut() {
        if packet.meta().discard() {
            continue;
        }

        let duplicate = shred::wire::get_shred(packet.as_ref())
            .map(|shred| ctx.deduper.dedup(shred))
            .unwrap_or(true);

        if duplicate && !packet.meta().repair() {
            packet.meta_mut().set_discard(true);
            counters.num_duplicates += 1;
        }
    }

    let banks = ctx.sharable_banks.load();
    let working_bank = banks.working_bank;
    let root_bank = banks.root_bank;

    let self_pubkey = keypair.pubkey();
    let slot_leaders = get_slot_leaders(
        &self_pubkey,
        batch,
        ctx.leader_schedule_cache.as_ref(),
        working_bank.as_ref(),
    )
    .filter_map(|(slot, pubkey)| pubkey.map(|pubkey| (slot, pubkey)))
    .chain(std::iter::once((Slot::MAX, Pubkey::default())))
    .collect::<SlotPubkeys>();

    for mut packet in batch.iter_mut() {
        if packet.meta().discard() {
            counters.num_discards_post += 1;
            continue;
        }

        if !verify_shred_cpu(packet.as_ref(), &slot_leaders, ctx.cache.as_ref()) {
            packet.meta_mut().set_discard(true);
            counters.num_discards_post += 1;
            continue;
        }

        if maybe_verify_and_resign_packet(
            &mut packet,
            root_bank.as_ref(),
            working_bank.as_ref(),
            ctx.cluster_info.as_ref(),
            ctx.leader_schedule_cache.as_ref(),
            ctx.cluster_nodes_cache.as_ref(),
            &mut counters,
            keypair,
        )
        .is_err()
        {
            packet.meta_mut().set_discard(true);
        }
    }

    counters
}

struct BatchJob {
    batch: PacketBatch,
    keypair: Arc<Keypair>,
}

#[derive(Debug)]
struct WorkerPanicked;

struct BatchResult {
    batch: PacketBatch,
    counters: WorkerCounters,
}

type WorkerResult = Result<BatchResult, WorkerPanicked>;

struct WorkerHandles(Vec<JoinHandle<()>>);

impl Drop for WorkerHandles {
    fn drop(&mut self) {
        for handle in self.0.drain(..) {
            if let Err(err) = handle.join() {
                error!("shred sigverify worker encountered unexpected error: {err:?}");
            }
        }
    }
}

struct ShredSigverifyWorkers {
    // Drop order is significant:
    // 1. Disconnect the job channel so workers stop receiving new work.
    // 2. Disconnect the result channel so workers cannot block sending results.
    // 3. Drop WorkerHandles, whose Drop impl joins all worker threads.
    job_sender: Sender<BatchJob>,
    result_receiver: Receiver<WorkerResult>,
    _worker_handles: WorkerHandles,
}

impl ShredSigverifyWorkers {
    fn new(
        num_workers: NonZeroUsize,
        cache: Arc<RwLock<LruCache>>,
        deduper: Arc<Deduper<2, [u8]>>,
        cluster_info: Arc<ClusterInfo>,
        bank_forks: Arc<RwLock<BankForks>>,
        leader_schedule_cache: Arc<LeaderScheduleCache>,
        cluster_nodes_cache: Arc<ClusterNodesCache<RetransmitStage>>,
    ) -> Self {
        let (job_sender, job_receiver) = bounded::<BatchJob>(SIGVERIFY_SHRED_BATCH_SIZE);
        let (result_sender, result_receiver) = bounded::<WorkerResult>(SIGVERIFY_SHRED_BATCH_SIZE);

        // Keep cheap lock-free handles to the current root and working banks.
        let sharable_banks = bank_forks.read().unwrap().sharable_banks();

        let worker_context = WorkerContext {
            cache,
            cluster_info,
            sharable_banks,
            leader_schedule_cache,
            cluster_nodes_cache,
            deduper,
        };

        let _worker_handles = WorkerHandles(
            (0..num_workers.get())
                .map(|index| {
                    let job_receiver = job_receiver.clone();
                    let result_sender = result_sender.clone();
                    let ctx = worker_context.clone();

                    Builder::new()
                        .name(format!("solSvrfyShred{index:02}"))
                        .spawn(move || {
                            defer! {
                                if !thread::panicking() {
                                    return;
                                }

                                let current_thread = thread::current();
                                error!("shred sigverify worker is panicking: {current_thread:?}");

                                if result_sender.send(Err(WorkerPanicked)).is_err() {
                                    error!("failed to report shred sigverify worker panic");
                                }
                            }

                            while let Ok(BatchJob { mut batch, keypair }) = job_receiver.recv() {
                                let counters = verify_batch(&mut batch, &ctx, keypair.as_ref());

                                if result_sender
                                    .send(Ok(BatchResult { batch, counters }))
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        })
                        .unwrap()
                })
                .collect(),
        );

        Self {
            job_sender,
            result_receiver,
            _worker_handles,
        }
    }

    fn process_batches(
        &self,
        packets: &mut Vec<PacketBatch>,
        keypair: &Arc<Keypair>,
    ) -> WorkerStats {
        if packets.is_empty() {
            return WorkerStats::default();
        }

        debug_assert!(
            packets.len() <= SIGVERIFY_SHRED_BATCH_SIZE,
            "run_shred_sigverify collects one batch plus at most SIGVERIFY_SHRED_BATCH_SIZE - 1 \
             additional batches"
        );

        let num_batches = packets.len();

        for batch in packets.drain(..) {
            self.job_sender
                .send(BatchJob {
                    batch,
                    keypair: keypair.clone(),
                })
                .expect("shred sigverify workers must be alive");
        }

        let mut worker_stats = WorkerStats::default();

        for _ in 0..num_batches {
            match self.result_receiver.recv() {
                Ok(Ok(BatchResult { batch, counters })) => {
                    worker_stats.add_batch(counters);
                    packets.push(batch);
                }
                Ok(Err(WorkerPanicked)) => {
                    panic!("shred sigverify worker panicked");
                }
                Err(_) => {
                    panic!("all shred sigverify workers died");
                }
            }
        }

        worker_stats
    }
}

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

    let cache = Arc::new(RwLock::new(LruCache::new(SIGVERIFY_LRU_CACHE_CAPACITY)));

    let deduper = {
        let mut rng = rand::rng();
        Arc::new(Deduper::<2, [u8]>::new(&mut rng, DEDUPER_NUM_BITS))
    };

    let cluster_nodes_cache = Arc::new(ClusterNodesCache::<RetransmitStage>::new(
        CLUSTER_NODES_CACHE_NUM_EPOCH_CAP,
        CLUSTER_NODES_CACHE_TTL,
    ));

    let workers = ShredSigverifyWorkers::new(
        num_sigverify_threads,
        cache,
        deduper.clone(),
        cluster_info.clone(),
        bank_forks.clone(),
        leader_schedule_cache.clone(),
        cluster_nodes_cache,
    );

    let run_shred_sigverify = move || {
        let mut rng = rand::rng();
        let mut shred_buffer = Vec::with_capacity(SIGVERIFY_SHRED_BATCH_SIZE);

        loop {
            if deduper.maybe_reset(&mut rng, DEDUPER_FALSE_POSITIVE_RATE, DEDUPER_RESET_CYCLE) {
                stats.num_deduper_saturations += 1;
            }

            // We can't store the keypair outside the loop
            // because the identity might be hot swapped.
            let keypair = cluster_info.keypair();

            match run_shred_sigverify(
                &workers,
                &keypair,
                &shred_fetch_receiver,
                &retransmit_sender,
                &verified_sender,
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
fn run_shred_sigverify(
    workers: &ShredSigverifyWorkers,
    keypair: &Arc<Keypair>,
    shred_fetch_receiver: &Receiver<PacketBatch>,
    retransmit_sender: &EvictingSender<Vec<shred::Payload>>,
    verified_sender: &Sender<Vec<(shred::Payload, /*is_repaired:*/ bool, BlockLocation)>>,
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

    let verify_and_resign_start = Instant::now();

    let worker_stats = workers.process_batches(shred_buffer, keypair);
    stats.add_worker_stats(worker_stats);

    stats.verify_and_resign_micros += verify_and_resign_start.elapsed().as_micros() as u64;

    let mut retransmit_shreds = Vec::new();
    let mut verified_shreds = Vec::new();

    for packet in shred_buffer
        .iter()
        .flat_map(|batch| batch.iter())
        .filter(|packet| !packet.meta().discard())
    {
        let Some((shred, location)) =
            extract_shred_and_location(packet, repair_nonce_location_lookup, stats)
        else {
            continue;
        };

        if let Some(location) = location {
            verified_shreds.push((shred, /* is_repaired */ true, location));
        } else {
            retransmit_shreds.push(shred.clone());
            verified_shreds.push((shred, /* is_repaired */ false, BlockLocation::Original));
        }
    }

    stats.num_retransmit_shreds += retransmit_shreds.len();

    if let Err(send_err) = retransmit_sender.try_send(retransmit_shreds) {
        match send_err {
            crossbeam_channel::TrySendError::Full(shreds) => {
                stats.num_retransmit_stage_overflow_shreds += shreds.len();
            }
            _ => unreachable!("EvictingSender holds on to both ends of the channel"),
        }
    }

    verified_sender.send(verified_shreds)?;

    stats.elapsed_micros += now.elapsed().as_micros() as u64;
    shred_buffer.clear();

    Ok(())
}

fn extract_shred_and_location(
    packet: PacketRef,
    repair_nonce_location_lookup: &RepairNonceLocationLookup,
    stats: &mut ShredSigVerifyStats,
) -> Option<(shred::Payload, Option<BlockLocation>)> {
    let (shred_bytes, nonce) = shred::layout::get_shred_and_repair_nonce(packet)?;

    let location = match nonce {
        None => None,
        Some(nonce) => match repair_nonce_location_lookup(nonce) {
            Some(location) => Some(location),
            None => {
                stats.num_unknown_block_location += 1;
                return None;
            }
        },
    };

    let payload = match packet {
        PacketRef::Packet(_) => shred::Payload::from(shred_bytes.to_vec()),
        PacketRef::Bytes(packet) => {
            shred::Payload::from(packet.buffer().slice(..shred_bytes.len()))
        }
    };

    Some((payload, location))
}
/// Checks whether the shred in `packet` uses a retransmitter-signed variant.
/// For turbine shreds, checks the retransmitter signature, then resigns the
/// packet with this node's keypair.
fn maybe_verify_and_resign_packet(
    packet: &mut PacketRefMut,
    root_bank: &Bank,
    working_bank: &Bank,
    cluster_info: &ClusterInfo,
    leader_schedule_cache: &LeaderScheduleCache,
    cluster_nodes_cache: &ClusterNodesCache<RetransmitStage>,
    counters: &mut WorkerCounters,
    keypair: &Keypair,
) -> Result<(), ResignError> {
    let repair = packet.meta().repair();
    let shred = get_shred(packet.as_ref()).ok_or(shred::Error::InvalidPacketSize)?;

    if !is_retransmitter_signed_variant(shred)? {
        return Ok(());
    }

    // Repair packets do not follow the turbine tree and are verified
    // using the trailing repair nonce.
    if !repair
        && !verify_retransmitter_signature(
            shred,
            root_bank,
            working_bank,
            cluster_info,
            leader_schedule_cache,
            cluster_nodes_cache,
            counters,
        )
    {
        counters.num_invalid_retransmitter += 1;

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
    counters: &mut WorkerCounters,
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
        counters.num_unknown_slot_leader += 1;
        return false;
    };

    let cluster_nodes =
        cluster_nodes_cache.get(shred.slot(), root_bank, working_bank, cluster_info);

    let parent = match cluster_nodes.get_retransmit_parent(&leader.id, &shred, DATA_PLANE_FANOUT) {
        Ok(Some(parent)) => parent,
        Ok(None) => {
            counters.num_retranmitter_signature_skipped += 1;
            return true;
        }
        Err(err) => {
            error!("get_retransmit_parent: {err:?}");
            counters.num_unknown_turbine_parent += 1;
            return false;
        }
    };

    if signature.verify(parent.as_ref(), merkle_root.as_ref()) {
        counters.num_retranmitter_signature_verified += 1;
        true
    } else {
        false
    }
}

// Returns pubkey of leaders for shred slots referenced in the packets.
// Marks packets as discard if:
//   - fails to deserialize the shred slot.
//   - slot leader is unknown.
//   - slot leader is the node itself (circular transmission).
fn get_slot_leaders<'a>(
    self_pubkey: &'a Pubkey,
    batch: &'a mut PacketBatch,
    leader_schedule_cache: &'a LeaderScheduleCache,
    bank: &'a Bank,
) -> impl Iterator<Item = (Slot, Option<Pubkey>)> + 'a {
    batch
        .iter_mut()
        .filter(|packet| !packet.meta().discard())
        .filter_map(move |mut packet| {
            let shred = shred::layout::get_shred(packet.as_ref());
            let slot = shred.and_then(shred::layout::get_slot)?;
            let leader = leader_schedule_cache
                .slot_leader_at(slot, Some(bank))
                .map(|leader| leader.id)
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
    num_discards_pre: usize,
    worker_stats: WorkerStats,
    num_retransmit_stage_overflow_shreds: usize,
    num_retransmit_shreds: usize,
    /// This means the OutstandingRequests cache is saturated and we
    /// threw away a verified shred due to being unable to fetch the storage location
    num_unknown_block_location: usize,
    elapsed_micros: u64,
    verify_and_resign_micros: u64,
}

impl ShredSigVerifyStats {
    const METRICS_SUBMIT_CADENCE: Duration = Duration::from_secs(2);

    fn new(now: Instant) -> Self {
        Self {
            since: now,
            num_iters: 0,
            num_batches: 0,
            num_packets: 0,
            num_deduper_saturations: 0,
            num_discards_pre: 0,
            worker_stats: WorkerStats::default(),
            num_retransmit_stage_overflow_shreds: 0,
            num_retransmit_shreds: 0,
            num_unknown_block_location: 0,
            elapsed_micros: 0,
            verify_and_resign_micros: 0,
        }
    }

    fn add_worker_stats(&mut self, worker_stats: WorkerStats) {
        self.worker_stats.num_discards_post += worker_stats.num_discards_post;
        self.worker_stats.num_duplicates += worker_stats.num_duplicates;
        self.worker_stats.num_invalid_retransmitter += worker_stats.num_invalid_retransmitter;
        self.worker_stats.num_retranmitter_signature_skipped +=
            worker_stats.num_retranmitter_signature_skipped;
        self.worker_stats.num_retranmitter_signature_verified +=
            worker_stats.num_retranmitter_signature_verified;
        self.worker_stats.num_unknown_slot_leader += worker_stats.num_unknown_slot_leader;
        self.worker_stats.num_unknown_turbine_parent += worker_stats.num_unknown_turbine_parent;
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
            (
                "num_discards_post",
                self.worker_stats.num_discards_post,
                i64
            ),
            ("num_duplicates", self.worker_stats.num_duplicates, i64),
            (
                "num_invalid_retransmitter",
                self.worker_stats.num_invalid_retransmitter,
                i64
            ),
            (
                "num_retranmitter_signature_skipped",
                self.worker_stats.num_retranmitter_signature_skipped,
                i64
            ),
            (
                "num_retranmitter_signature_verified",
                self.worker_stats.num_retranmitter_signature_verified,
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
                self.worker_stats.num_unknown_slot_leader,
                i64
            ),
            (
                "num_unknown_turbine_parent",
                self.worker_stats.num_unknown_turbine_parent,
                i64
            ),
            ("elapsed_micros", self.elapsed_micros, i64),
            (
                "verify_and_resign_micros",
                self.verify_and_resign_micros,
                i64
            ),
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

    fn new_sigverify_workers(
        cluster_info: Arc<ClusterInfo>,
        bank_forks: Arc<RwLock<BankForks>>,
        leader_schedule_cache: Arc<LeaderScheduleCache>,
        num_workers: usize,
    ) -> ShredSigverifyWorkers {
        let cache = Arc::new(RwLock::new(LruCache::new(/*capacity:*/ 128)));

        new_sigverify_workers_with_cache(
            cluster_info,
            bank_forks,
            leader_schedule_cache,
            cache,
            num_workers,
        )
    }
    // Allows panic-path tests to inject a poisoned cache.
    fn new_sigverify_workers_with_cache(
        cluster_info: Arc<ClusterInfo>,
        bank_forks: Arc<RwLock<BankForks>>,
        leader_schedule_cache: Arc<LeaderScheduleCache>,
        cache: Arc<RwLock<LruCache>>,
        num_workers: usize,
    ) -> ShredSigverifyWorkers {
        let deduper = {
            let mut rng = rand::rng();
            Arc::new(Deduper::<2, [u8]>::new(&mut rng, DEDUPER_NUM_BITS))
        };

        let cluster_nodes_cache = Arc::new(ClusterNodesCache::<RetransmitStage>::new(
            CLUSTER_NODES_CACHE_NUM_EPOCH_CAP,
            CLUSTER_NODES_CACHE_TTL,
        ));

        ShredSigverifyWorkers::new(
            NonZeroUsize::new(num_workers).unwrap(),
            cache,
            deduper,
            cluster_info,
            bank_forks,
            leader_schedule_cache,
            cluster_nodes_cache,
        )
    }
    #[test]
    #[should_panic(expected = "shred sigverify worker panicked")]
    fn test_sigverify_worker_panic_is_propagated() {
        let leader_keypair = Arc::new(Keypair::new());
        let node_keypair = Arc::new(Keypair::new());
        let leader_pubkey = leader_keypair.pubkey();
        let node_pubkey = node_keypair.pubkey();

        let bank = Bank::new_for_tests(
            &create_genesis_config_with_leader(100, &leader_pubkey, 10).genesis_config,
        );
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let bank_forks = BankForks::new_rw_arc(bank);

        let cluster_info = Arc::new(ClusterInfo::new(
            ContactInfo::new_localhost(&node_pubkey, timestamp()),
            node_keypair.clone(),
            SocketAddrSpace::Unspecified,
        ));

        let cache = Arc::new(RwLock::new(LruCache::new(/*capacity:*/ 128)));
        let cache_to_poison = cache.clone();

        let poison_result = std::thread::spawn(move || {
            let _guard = cache_to_poison.write().unwrap();
            panic!("poison shred sigverify cache");
        })
        .join();

        assert!(poison_result.is_err());

        let workers = new_sigverify_workers_with_cache(
            cluster_info,
            bank_forks,
            leader_schedule_cache,
            cache,
            1,
        );

        let entries = create_ticks(1, 1, Hash::new_unique());
        let shredder = Shredder::new(1, 0, 1, 0).unwrap();

        let (shreds, _) = shredder.entries_to_merkle_shreds_for_tests(
            &leader_keypair,
            &entries,
            false,
            Hash::new_unique(),
            0,
            0,
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        );

        let shred = &shreds[0];
        let mut batch = RecycledPacketBatch::with_capacity(1);
        batch.resize(1, Packet::default());
        batch[0].buffer_mut()[..shred.payload().len()].copy_from_slice(shred.payload());
        batch[0].meta_mut().size = shred.payload().len();

        let mut batches = vec![PacketBatch::from(batch)];

        workers.process_batches(&mut batches, &node_keypair);
    }

    #[test]
    fn test_sigverify_shreds_verify_batches() {
        let leader_keypair = Arc::new(Keypair::new());
        let wrong_keypair = Keypair::new();
        let node_keypair = Arc::new(Keypair::new());
        let leader_pubkey = leader_keypair.pubkey();
        let node_pubkey = node_keypair.pubkey();

        let bank = Bank::new_for_tests(
            &create_genesis_config_with_leader(100, &leader_pubkey, 10).genesis_config,
        );
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let bank_forks = BankForks::new_rw_arc(bank);

        let cluster_info = Arc::new(ClusterInfo::new(
            ContactInfo::new_localhost(&node_pubkey, timestamp()),
            node_keypair.clone(),
            SocketAddrSpace::Unspecified,
        ));

        let workers = new_sigverify_workers(cluster_info, bank_forks, leader_schedule_cache, 3);

        let entries = create_ticks(1, 1, Hash::new_unique());
        let shredder = Shredder::new(1, 0, 1, 0).unwrap();

        let (valid_shreds, _) = shredder.entries_to_merkle_shreds_for_tests(
            &leader_keypair,
            &entries,
            false,
            Hash::new_unique(),
            0,
            0,
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        );

        let (invalid_shreds, _) = shredder.entries_to_merkle_shreds_for_tests(
            &wrong_keypair,
            &entries,
            false,
            Hash::new_unique(),
            0,
            0,
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        );

        let mut batch = RecycledPacketBatch::with_capacity(2);
        batch.resize(2, Packet::default());

        let shred = &valid_shreds[0];
        batch[0].buffer_mut()[..shred.payload().len()].copy_from_slice(shred.payload());
        batch[0].meta_mut().size = shred.payload().len();

        let shred = &invalid_shreds[0];
        batch[1].buffer_mut()[..shred.payload().len()].copy_from_slice(shred.payload());
        batch[1].meta_mut().size = shred.payload().len();

        let mut batches = vec![PacketBatch::from(batch)];

        let worker_stats = workers.process_batches(&mut batches, &node_keypair);

        assert_eq!(worker_stats.num_discards_post, 1);
        assert!(!batches[0].get(0).unwrap().meta().discard());
        assert!(batches[0].get(1).unwrap().meta().discard());
    }

    #[test_matrix([true, false], [true, false])]
    fn test_maybe_verify_and_resign_packet(repaired: bool, is_last_in_slot: bool) {
        let mut rng = rand::rng();

        let leader_keypair = Arc::new(Keypair::new());
        let leader_pubkey = leader_keypair.pubkey();

        let bank = Bank::new_for_tests(
            &create_genesis_config_with_leader(100, &leader_pubkey, 10).genesis_config,
        );
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let bank_forks = BankForks::new_rw_arc(bank);

        let (working_bank, root_bank) = {
            let bank_forks = bank_forks.read().unwrap();
            (bank_forks.working_bank(), bank_forks.root_bank())
        };

        let cluster_info = Arc::new(ClusterInfo::new(
            ContactInfo::new_localhost(&leader_pubkey, timestamp()),
            leader_keypair.clone(),
            SocketAddrSpace::Unspecified,
        ));

        let cluster_nodes_cache = ClusterNodesCache::<RetransmitStage>::new(
            CLUSTER_NODES_CACHE_NUM_EPOCH_CAP,
            CLUSTER_NODES_CACHE_TTL,
        );

        let chained_merkle_root = Hash::new_from_array(rng.random());
        let shredder = Shredder::new(root_bank.slot(), root_bank.parent_slot(), 0, 0).unwrap();
        let entries = vec![Entry::new(&Hash::default(), 0, vec![])];
        let shreds = shredder.make_merkle_shreds_from_entries(
            &leader_keypair,
            &entries,
            is_last_in_slot,
            chained_merkle_root,
            0,
            0,
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        );

        for shred in &shreds {
            let retransmitter_keypair = Keypair::new();
            let nonce = repaired.then(|| rng.random::<Nonce>());

            let mut packet = shred.payload().to_packet(nonce);
            if repaired {
                packet.meta_mut().flags |= PacketFlags::REPAIR;
            }

            let mut packet_batch = RecycledPacketBatch::with_capacity(1);
            packet_batch.push(packet);

            let mut bytes_packet = shred.payload().to_bytes_packet(nonce);
            if repaired {
                bytes_packet.meta_mut().flags |= PacketFlags::REPAIR;
            }

            let bytes_buffer_address_before = bytes_packet.buffer().as_ptr().addr();

            let mut batches = vec![
                PacketBatch::from(packet_batch),
                PacketBatch::Single(bytes_packet),
            ];

            let packet_buffer_before = batches[0].get(0).unwrap().data(..).unwrap().to_vec();

            let mut counters = WorkerCounters::default();

            for batch in &mut batches {
                let mut packet = batch.get_mut(0).unwrap();

                maybe_verify_and_resign_packet(
                    &mut packet,
                    root_bank.as_ref(),
                    working_bank.as_ref(),
                    cluster_info.as_ref(),
                    leader_schedule_cache.as_ref(),
                    &cluster_nodes_cache,
                    &mut counters,
                    &retransmitter_keypair,
                )
                .unwrap();
            }

            let packet = batches
                .iter()
                .find_map(|batch| match batch {
                    PacketBatch::Single(_) => None,
                    batch => batch.get(0),
                })
                .unwrap();

            let bytes_packet = batches
                .iter()
                .find_map(|batch| match batch {
                    PacketBatch::Single(packet) => Some(packet),
                    _ => None,
                })
                .unwrap();

            assert!(!packet.meta().discard());
            assert!(!bytes_packet.meta().discard());

            let packet_buffer_after = packet.data(..).unwrap();
            let bytes_buffer_address_after = bytes_packet.buffer().as_ptr().addr();

            if is_last_in_slot {
                assert_ne!(packet_buffer_before.as_slice(), packet_buffer_after);
                assert_ne!(bytes_buffer_address_before, bytes_buffer_address_after);

                for batch in &batches {
                    let packet = batch.get(0).unwrap();
                    let shred = get_shred(packet).unwrap();
                    let signature = shred::layout::get_retransmitter_signature(shred).unwrap();
                    let merkle_root = shred::layout::get_merkle_root(shred).unwrap();

                    assert!(signature.verify(
                        retransmitter_keypair.pubkey().as_ref(),
                        merkle_root.as_ref(),
                    ));
                }
            } else {
                assert_eq!(packet_buffer_before.as_slice(), packet_buffer_after);
                assert_eq!(bytes_buffer_address_before, bytes_buffer_address_after);
            }
        }
    }

    #[test]
    fn test_extract_shred_and_location_bytes_packet_is_zero_copy() {
        let leader_keypair = Keypair::new();

        let entries = create_ticks(1, 1, Hash::new_unique());
        let shredder = Shredder::new(1, 0, 1, 0).unwrap();

        let (shreds, _) = shredder.entries_to_merkle_shreds_for_tests(
            &leader_keypair,
            &entries,
            false,
            Hash::new_unique(),
            0,
            0,
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        );

        let bytes_packet = shreds[0].payload().to_bytes_packet(None);

        let buffer_ptr = bytes_packet.buffer().as_ptr();

        let mut stats = ShredSigVerifyStats::new(Instant::now());
        let repair_nonce_location_lookup = |_| None;

        let (payload, location) = extract_shred_and_location(
            PacketRef::Bytes(&bytes_packet),
            &repair_nonce_location_lookup,
            &mut stats,
        )
        .unwrap();

        assert!(location.is_none());
        assert_eq!(payload.bytes.as_ptr(), buffer_ptr);
        assert_eq!(payload.bytes.as_ref(), shreds[0].payload().bytes.as_ref());
    }

    #[test]
    fn test_sigverify_workers_fanout_aggregates_stats() {
        const NUM_WORKERS: usize = 3;
        const NUM_BATCHES: usize = SIGVERIFY_SHRED_BATCH_SIZE;

        let leader_keypair = Arc::new(Keypair::new());
        let wrong_keypair = Keypair::new();
        let node_keypair = Arc::new(Keypair::new());
        let leader_pubkey = leader_keypair.pubkey();
        let node_pubkey = node_keypair.pubkey();

        let bank = Bank::new_for_tests(
            &create_genesis_config_with_leader(100, &leader_pubkey, 10).genesis_config,
        );
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let bank_forks = BankForks::new_rw_arc(bank);

        let cluster_info = Arc::new(ClusterInfo::new(
            ContactInfo::new_localhost(&node_pubkey, timestamp()),
            node_keypair.clone(),
            SocketAddrSpace::Unspecified,
        ));

        let workers =
            new_sigverify_workers(cluster_info, bank_forks, leader_schedule_cache, NUM_WORKERS);

        let entries = create_ticks(1, 1, Hash::new_unique());
        let shredder = Shredder::new(1, 0, 1, 0).unwrap();
        let reed_solomon_cache = ReedSolomonCache::default();

        let mut batches = Vec::with_capacity(NUM_BATCHES);

        for index in 0..NUM_BATCHES {
            let signing_keypair = if index % 2 == 0 {
                leader_keypair.as_ref()
            } else {
                &wrong_keypair
            };

            // Use a different chained Merkle root for every batch so that
            // otherwise-valid shreds are not treated as duplicates.
            let (shreds, _) = shredder.entries_to_merkle_shreds_for_tests(
                signing_keypair,
                &entries,
                false,
                Hash::new_unique(),
                0,
                0,
                &reed_solomon_cache,
                &mut ProcessShredsStats::default(),
            );

            let shred = &shreds[0];

            let mut batch = RecycledPacketBatch::with_capacity(1);
            batch.resize(1, Packet::default());
            batch[0].buffer_mut()[..shred.payload().len()].copy_from_slice(shred.payload());
            batch[0].meta_mut().size = shred.payload().len();

            batches.push(PacketBatch::from(batch));
        }

        let worker_stats = workers.process_batches(&mut batches, &node_keypair);

        assert_eq!(batches.len(), NUM_BATCHES);

        let num_discards = batches
            .iter()
            .flat_map(|batch| batch.iter())
            .filter(|packet| packet.meta().discard())
            .count();

        assert_eq!(num_discards, NUM_BATCHES / 2);
        assert_eq!(worker_stats.num_discards_post, NUM_BATCHES / 2);
        assert_eq!(worker_stats.num_duplicates, 0);
    }
    #[test]
    fn test_sigverify_workers_dedup_across_batches() {
        const NUM_WORKERS: usize = 8;
        const NUM_BATCHES: usize = 64;

        let leader_keypair = Arc::new(Keypair::new());
        let node_keypair = Arc::new(Keypair::new());
        let leader_pubkey = leader_keypair.pubkey();
        let node_pubkey = node_keypair.pubkey();

        let bank = Bank::new_for_tests(
            &create_genesis_config_with_leader(100, &leader_pubkey, 10).genesis_config,
        );
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let bank_forks = BankForks::new_rw_arc(bank);

        let cluster_info = Arc::new(ClusterInfo::new(
            ContactInfo::new_localhost(&node_pubkey, timestamp()),
            node_keypair.clone(),
            SocketAddrSpace::Unspecified,
        ));

        let workers =
            new_sigverify_workers(cluster_info, bank_forks, leader_schedule_cache, NUM_WORKERS);

        let entries = create_ticks(1, 1, Hash::new_unique());
        let shredder = Shredder::new(1, 0, 1, 0).unwrap();

        let (shreds, _) = shredder.entries_to_merkle_shreds_for_tests(
            &leader_keypair,
            &entries,
            false,
            Hash::new_unique(),
            0,
            0,
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        );

        let shred = &shreds[0];

        let mut batches = Vec::with_capacity(NUM_BATCHES);

        for _ in 0..NUM_BATCHES {
            let mut batch = RecycledPacketBatch::with_capacity(1);
            batch.resize(1, Packet::default());

            batch[0].buffer_mut()[..shred.payload().len()].copy_from_slice(shred.payload());
            batch[0].meta_mut().size = shred.payload().len();

            batches.push(PacketBatch::from(batch));
        }

        let worker_stats = workers.process_batches(&mut batches, &node_keypair);

        assert_eq!(batches.len(), NUM_BATCHES);

        let num_discards = batches
            .iter()
            .flat_map(|batch| batch.iter())
            .filter(|packet| packet.meta().discard())
            .count();

        let num_survivors = NUM_BATCHES - num_discards;

        // Deduper<2> sets two atomic bits for each payload. Concurrent workers can
        // race while setting those bits, so at most two copies may be accepted.
        assert!((1..=2).contains(&num_survivors));

        assert_eq!(worker_stats.num_duplicates, num_discards);
        assert_eq!(worker_stats.num_discards_post, num_discards);
    }
    #[test]
    fn test_sigverify_workers_repair_shreds_are_deduped_but_not_discarded() {
        let leader_keypair = Arc::new(Keypair::new());
        let node_keypair = Arc::new(Keypair::new());
        let leader_pubkey = leader_keypair.pubkey();
        let node_pubkey = node_keypair.pubkey();

        let bank = Bank::new_for_tests(
            &create_genesis_config_with_leader(100, &leader_pubkey, 10).genesis_config,
        );
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let bank_forks = BankForks::new_rw_arc(bank);

        let cluster_info = Arc::new(ClusterInfo::new(
            ContactInfo::new_localhost(&node_pubkey, timestamp()),
            node_keypair.clone(),
            SocketAddrSpace::Unspecified,
        ));

        let workers = new_sigverify_workers(cluster_info, bank_forks, leader_schedule_cache, 3);

        let entries = create_ticks(1, 1, Hash::new_unique());
        let shredder = Shredder::new(1, 0, 1, 0).unwrap();
        let reed_solomon_cache = ReedSolomonCache::default();

        let (shreds_a, _) = shredder.entries_to_merkle_shreds_for_tests(
            &leader_keypair,
            &entries,
            false,
            Hash::new_unique(),
            0,
            0,
            &reed_solomon_cache,
            &mut ProcessShredsStats::default(),
        );

        let (shreds_b, _) = shredder.entries_to_merkle_shreds_for_tests(
            &leader_keypair,
            &entries,
            false,
            Hash::new_unique(),
            0,
            0,
            &reed_solomon_cache,
            &mut ProcessShredsStats::default(),
        );

        let shred_a = &shreds_a[0];
        let shred_b = &shreds_b[0];

        let mut batch = RecycledPacketBatch::with_capacity(4);
        batch.resize(4, Packet::default());

        // Turbine A: first time the deduper sees payload A.
        batch[0].buffer_mut()[..shred_a.payload().len()].copy_from_slice(shred_a.payload());
        batch[0].meta_mut().size = shred_a.payload().len();

        // Repair A: duplicate of A, but repair packets are exempt from discard.
        batch[1].buffer_mut()[..shred_a.payload().len()].copy_from_slice(shred_a.payload());
        batch[1].meta_mut().size = shred_a.payload().len();
        batch[1].meta_mut().flags |= PacketFlags::REPAIR;

        // Repair B: first time the deduper sees payload B. It must still be fed
        // into the deduper even though repair packets are exempt from discard.
        batch[2].buffer_mut()[..shred_b.payload().len()].copy_from_slice(shred_b.payload());
        batch[2].meta_mut().size = shred_b.payload().len();
        batch[2].meta_mut().flags |= PacketFlags::REPAIR;

        // Turbine B: must now be discarded because repair B inserted the same
        // shred payload into the deduper.
        batch[3].buffer_mut()[..shred_b.payload().len()].copy_from_slice(shred_b.payload());
        batch[3].meta_mut().size = shred_b.payload().len();

        let mut batches = vec![PacketBatch::from(batch)];

        let worker_stats = workers.process_batches(&mut batches, &node_keypair);

        let batch = &batches[0];

        assert!(!batch.get(0).unwrap().meta().discard());
        assert!(!batch.get(1).unwrap().meta().discard());
        assert!(!batch.get(2).unwrap().meta().discard());
        assert!(batch.get(3).unwrap().meta().discard());

        assert_eq!(worker_stats.num_duplicates, 1);
        assert_eq!(worker_stats.num_discards_post, 1);
    }
    #[test]
    fn test_sigverify_workers_use_updated_keypair_across_rounds() {
        let mut rng = rand::rng();

        let leader_keypair = Arc::new(Keypair::new());
        let node_keypair_a = Arc::new(Keypair::new());
        let node_keypair_b = Arc::new(Keypair::new());

        let leader_pubkey = leader_keypair.pubkey();
        let node_pubkey = node_keypair_a.pubkey();

        let bank = Bank::new_for_tests(
            &create_genesis_config_with_leader(100, &leader_pubkey, 10).genesis_config,
        );
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let bank_forks = BankForks::new_rw_arc(bank);

        let cluster_info = Arc::new(ClusterInfo::new(
            ContactInfo::new_localhost(&node_pubkey, timestamp()),
            node_keypair_a.clone(),
            SocketAddrSpace::Unspecified,
        ));

        let workers = new_sigverify_workers(cluster_info, bank_forks, leader_schedule_cache, 3);

        let entries = vec![Entry::new(&Hash::default(), 0, vec![])];
        let shredder = Shredder::new(1, 0, 0, 0).unwrap();

        // Round 1 uses node_keypair_a.
        let shreds = shredder.make_merkle_shreds_from_entries(
            &leader_keypair,
            &entries,
            true,
            Hash::new_unique(),
            0,
            0,
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        );

        let shred = &shreds[0];
        let nonce = rng.random::<Nonce>();
        let mut packet = shred.payload().to_packet(Some(nonce));
        packet.meta_mut().flags |= PacketFlags::REPAIR;

        let mut batch = RecycledPacketBatch::with_capacity(1);
        batch.push(packet);

        let mut batches = vec![PacketBatch::from(batch)];

        workers.process_batches(&mut batches, &node_keypair_a);

        let packet = batches[0].get(0).unwrap();
        assert!(!packet.meta().discard());

        let shred = get_shred(packet).unwrap();
        let signature = shred::layout::get_retransmitter_signature(shred).unwrap();
        let merkle_root = shred::layout::get_merkle_root(shred).unwrap();

        assert!(signature.verify(node_keypair_a.pubkey().as_ref(), merkle_root.as_ref(),));

        // Round 2 reuses the same workers but supplies node_keypair_b.
        // Use a different shred so the shared deduper does not discard it.
        let shreds = shredder.make_merkle_shreds_from_entries(
            &leader_keypair,
            &entries,
            true,
            Hash::new_unique(),
            0,
            0,
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        );

        let shred = &shreds[0];
        let nonce = rng.random::<Nonce>();
        let mut packet = shred.payload().to_packet(Some(nonce));
        packet.meta_mut().flags |= PacketFlags::REPAIR;

        let mut batch = RecycledPacketBatch::with_capacity(1);
        batch.push(packet);

        let mut batches = vec![PacketBatch::from(batch)];

        workers.process_batches(&mut batches, &node_keypair_b);

        let packet = batches[0].get(0).unwrap();
        assert!(!packet.meta().discard());

        let shred = get_shred(packet).unwrap();
        let signature = shred::layout::get_retransmitter_signature(shred).unwrap();
        let merkle_root = shred::layout::get_merkle_root(shred).unwrap();

        assert!(signature.verify(node_keypair_b.pubkey().as_ref(), merkle_root.as_ref(),));
        assert!(!signature.verify(node_keypair_a.pubkey().as_ref(), merkle_root.as_ref(),));
    }
}
