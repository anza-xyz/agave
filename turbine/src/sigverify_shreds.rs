use {
    agave_wake_channel::{Receiver as WakeReceiver, Sender as WakeSender, bounded as wake_bounded},
    crossbeam_channel::{Receiver, RecvTimeoutError, SendError, Sender},
    scopeguard::defer,
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::{
        blockstore_meta::BlockLocation,
        leader_schedule_cache::LeaderScheduleCache,
        shred::{
            self,
            layout::{is_retransmitter_signed_variant, set_retransmitter_signature},
        },
        sigverify_shreds::{LruCache, SlotPubkeys, verify_shred_cpu},
    },
    solana_perf::{
        deduper::Deduper,
        packet::{BytesPacket, PacketBatch},
    },
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::Bank,
        bank_forks::{BankForks, SharableBanks},
    },
    solana_signature::Signature,
    solana_signer::Signer,
    solana_streamer::{evicting_sender::EvictingSender, streamer::ChannelSend},
    std::{
        num::NonZeroUsize,
        sync::{Arc, RwLock},
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

// 34MB where each cache entry is 136 bytes.
const SIGVERIFY_LRU_CACHE_CAPACITY: usize = 1 << 18;

const DEDUPER_FALSE_POSITIVE_RATE: f64 = 0.001;
const DEDUPER_NUM_BITS: u64 = 637_534_199; // 76MB
const DEDUPER_RESET_CYCLE: Duration = Duration::from_secs(5 * 60);

/// Maximum number of packet batches processed in a single sigverify iteration.
///
/// In case of legitimate sigverify traffic sigverify stage keeps up with fetch stage and processes
/// one packet batch per iteration. If a backlog accumulates, this limits each iteration to
/// `SIGVERIFY_SHRED_BATCH_SIZE` batches, each up to `PACKETS_PER_BATCH` shreds.
const SIGVERIFY_SHRED_BATCH_SIZE: usize = 128;

#[allow(clippy::enum_variant_names)]
enum ShredSigverifyError {
    RecvDisconnected,
    RecvTimeout,
    SendError,
}

pub type RepairNonceLocationLookup = dyn Fn(shred::Nonce) -> Option<BlockLocation> + Send + Sync;

#[derive(Default)]
struct WorkerStats {
    num_discards_post: usize,
    num_duplicates: usize,
}

impl WorkerStats {
    fn accumulate(&mut self, stats: Self) {
        self.num_discards_post += stats.num_discards_post;
        self.num_duplicates += stats.num_duplicates;
    }

    fn add_batch(&mut self, counters: WorkerCounters) {
        self.accumulate(Self {
            num_discards_post: usize::from(counters.num_discards_post),
            num_duplicates: usize::from(counters.num_duplicates),
        });
    }
}

#[derive(Default)]
struct WorkerCounters {
    num_discards_post: u16,
    num_duplicates: u16,
}

#[derive(Clone)]
struct WorkerContext {
    sigverify_cache: Arc<RwLock<LruCache>>,
    sharable_banks: SharableBanks,
    leader_schedule_cache: Arc<LeaderScheduleCache>,
    deduper: Arc<Deduper<2, [u8]>>,
}

fn verify_batch(
    batch: &mut PacketBatch,
    ctx: &WorkerContext,
    self_pubkey: &Pubkey,
) -> WorkerCounters {
    debug_assert!(batch.len() <= usize::from(u16::MAX));
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
    // The retransmitter signature is hop-specific, so it is also excluded from
    // the deduper.
    for packet in batch.iter_mut() {
        if packet.meta().discard() {
            continue;
        }

        let duplicate = shred::wire::get_shred_without_retransmitter_signature(packet)
            .map(|shred| ctx.deduper.dedup(shred))
            .unwrap_or(true);

        if duplicate && !packet.meta().repair() {
            packet.meta_mut().set_discard(true);
            counters.num_duplicates += 1;
        }
    }

    let working_bank = ctx.sharable_banks.load().working_bank;

    let slot_leaders = get_slot_leaders(
        self_pubkey,
        batch,
        ctx.leader_schedule_cache.as_ref(),
        working_bank.as_ref(),
    )
    .filter_map(|(slot, pubkey)| pubkey.map(|pubkey| (slot, pubkey)))
    .chain(std::iter::once((Slot::MAX, Pubkey::default())))
    .collect::<SlotPubkeys>();

    for packet in batch.iter_mut() {
        if packet.meta().discard() {
            counters.num_discards_post += 1;
            continue;
        }

        if !verify_shred_cpu(packet, &slot_leaders, ctx.sigverify_cache.as_ref()) {
            packet.meta_mut().set_discard(true);
            counters.num_discards_post += 1;
        }
    }

    counters
}

struct BatchJob {
    batch: PacketBatch,
    self_pubkey: Pubkey,
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
        let mut worker_panicked = false;

        for handle in self.0.drain(..) {
            worker_panicked |= handle.join().is_err();
        }

        // process_batches already propagates worker panics; avoid a second
        // panic if the owner is already unwinding.
        if worker_panicked && !thread::panicking() {
            panic!("shred sigverify worker panicked");
        }
    }
}

struct ShredSigverifyWorkers {
    // Drop order is significant:
    // 1. Disconnect the job channel so workers stop receiving new work.
    // 2. Disconnect the result channel so workers cannot block sending results.
    // 3. Drop WorkerHandles, whose Drop impl joins all worker threads.
    job_sender: WakeSender<BatchJob>,
    result_receiver: WakeReceiver<WorkerResult>,
    _worker_handles: WorkerHandles,
}

impl ShredSigverifyWorkers {
    fn new(
        num_workers: NonZeroUsize,
        sigverify_cache: Arc<RwLock<LruCache>>,
        deduper: Arc<Deduper<2, [u8]>>,
        bank_forks: Arc<RwLock<BankForks>>,
        leader_schedule_cache: Arc<LeaderScheduleCache>,
    ) -> Self {
        let num_workers = num_workers.get();

        let (job_sender, job_receiver) = wake_bounded::<BatchJob>(SIGVERIFY_SHRED_BATCH_SIZE);
        let (result_sender, result_receiver) =
            wake_bounded::<WorkerResult>(SIGVERIFY_SHRED_BATCH_SIZE);

        // Keep a cheap lock-free handle to the current working bank.
        let sharable_banks = bank_forks.read().unwrap().sharable_banks();

        let worker_context = WorkerContext {
            sigverify_cache,
            sharable_banks,
            leader_schedule_cache,
            deduper,
        };

        let worker_handles = WorkerHandles(
            (0..num_workers)
                .map(|index| {
                    let job_receiver = job_receiver.clone();
                    let result_sender = result_sender.clone();
                    let ctx = worker_context.clone();

                    Builder::new()
                        .name(format!("solSvrfyShred{index:02}"))
                        .spawn(move || {
                            defer! {
                                if thread::panicking() {
                                    result_sender
                                        .send(Err(WorkerPanicked))
                                        .expect("shred sigverify result receiver must remain alive while workers are running");
                                }
                            }

                            while let Ok(BatchJob {
                                mut batch,
                                self_pubkey,
                            }) = job_receiver.recv()
                            {
                                let counters = verify_batch(&mut batch, &ctx, &self_pubkey);

                                if result_sender
                                    .send(Ok(BatchResult { batch, counters }))
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        })
                        .expect("shred sigverify worker thread must spawn during pool construction")
                })
                .collect(),
        );

        Self {
            job_sender,
            result_receiver,
            _worker_handles: worker_handles,
        }
    }

    fn process_batches(&self, packets: &mut Vec<PacketBatch>, self_pubkey: &Pubkey) -> WorkerStats {
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
                    self_pubkey: *self_pubkey,
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

    let sigverify_cache = Arc::new(RwLock::new(LruCache::new(SIGVERIFY_LRU_CACHE_CAPACITY)));

    let deduper = {
        let mut rng = rand::rng();
        Arc::new(Deduper::<2, [u8]>::new(&mut rng, DEDUPER_NUM_BITS))
    };

    let workers = ShredSigverifyWorkers::new(
        num_sigverify_threads,
        sigverify_cache,
        deduper.clone(),
        bank_forks.clone(),
        leader_schedule_cache.clone(),
    );

    let run_shred_sigverify = move || {
        let mut rng = rand::rng();
        let mut shred_buffer = Vec::with_capacity(SIGVERIFY_SHRED_BATCH_SIZE);

        loop {
            if deduper.maybe_reset(&mut rng, DEDUPER_FALSE_POSITIVE_RATE, DEDUPER_RESET_CYCLE) {
                stats.num_deduper_saturations += 1;
            }

            // Read the identity on every iteration because it might be hot swapped.
            let self_pubkey = cluster_info.keypair().pubkey();

            match run_shred_sigverify(
                &workers,
                &self_pubkey,
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
        .expect("failed to spawn shred sigverify coordinator thread")
}

fn run_shred_sigverify(
    workers: &ShredSigverifyWorkers,
    self_pubkey: &Pubkey,
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

    let worker_stats = workers.process_batches(shred_buffer, self_pubkey);
    stats.worker_stats.accumulate(worker_stats);

    stats.verify_micros += now.elapsed().as_micros() as u64;

    let mut retransmit_shreds = Vec::new();
    let mut verified_shreds = Vec::new();

    // Extract shred payload from packets, and separate out repaired shreds.
    for packet in shred_buffer
        .iter()
        .flat_map(|batch| batch.iter())
        .filter(|packet| !packet.meta().discard())
    {
        let Some((mut shred, location)) =
            extract_shred_and_location(packet, repair_nonce_location_lookup, stats)
        else {
            continue;
        };

        if maybe_clear_retransmitter_signature(&mut shred).is_err() {
            continue;
        }

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
    packet: &BytesPacket,
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

    let payload = shred::Payload::from(packet.buffer().slice(..shred_bytes.len()));

    Some((payload, location))
}

/// Zeroes the retransmitter signature of the `shred` if it is of resigned variant.
///
/// `verify_retransmitter_signature` feature will never be activated, so we do not need to make
/// them. The signature is outside of the Merkle tree and the leader signed data, so its
/// contents do not affect shred validity. Writing zeros is ensuring all offsets are valid.
///
/// Overwriting the signature with zeros ensures we do not forward untrusted bytes.
fn maybe_clear_retransmitter_signature(shred: &mut shred::Payload) -> Result<(), shred::Error> {
    if is_retransmitter_signed_variant(shred.as_ref())? {
        let mut payload = shred.as_mut();
        set_retransmitter_signature(payload.as_mut(), &Signature::default())?;
    }
    Ok(())
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
        .filter_map(move |packet| {
            let shred = shred::layout::get_shred(packet);
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
    verify_micros: u64,
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
            verify_micros: 0,
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
            (
                "num_discards_post",
                self.worker_stats.num_discards_post,
                i64
            ),
            ("num_duplicates", self.worker_stats.num_duplicates, i64),
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
            ("elapsed_micros", self.elapsed_micros, i64),
            ("verify_micros", self.verify_micros, i64),
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
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_ledger::{
            genesis_utils::create_genesis_config_with_leader,
            shred::{ProcessShredsStats, Shredder, layout::get_retransmitter_signature},
        },
        solana_perf::packet::{BytesPacketBatch, PacketFlags},
        solana_runtime::bank::Bank,
        solana_signer::Signer,
        test_case::test_case,
    };

    // Keep the cache small for unit tests.
    const TEST_SIGVERIFY_LRU_CACHE_CAPACITY: usize = 128;

    fn new_sigverify_workers(
        bank_forks: Arc<RwLock<BankForks>>,
        leader_schedule_cache: Arc<LeaderScheduleCache>,
        num_workers: usize,
    ) -> ShredSigverifyWorkers {
        let sigverify_cache = Arc::new(RwLock::new(LruCache::new(
            TEST_SIGVERIFY_LRU_CACHE_CAPACITY,
        )));

        new_sigverify_workers_with_cache(
            bank_forks,
            leader_schedule_cache,
            sigverify_cache,
            num_workers,
        )
    }

    // Allows panic-path tests to inject a poisoned cache.
    fn new_sigverify_workers_with_cache(
        bank_forks: Arc<RwLock<BankForks>>,
        leader_schedule_cache: Arc<LeaderScheduleCache>,
        sigverify_cache: Arc<RwLock<LruCache>>,
        num_workers: usize,
    ) -> ShredSigverifyWorkers {
        let deduper = {
            let mut rng = rand::rng();
            Arc::new(Deduper::<2, [u8]>::new(&mut rng, DEDUPER_NUM_BITS))
        };

        ShredSigverifyWorkers::new(
            NonZeroUsize::new(num_workers).unwrap(),
            sigverify_cache,
            deduper,
            bank_forks,
            leader_schedule_cache,
        )
    }

    #[test]
    fn test_dedup_ignores_retransmitter_signature() {
        let mut rng = rand::rng();
        let leader_keypair = Keypair::new();
        let shredder = Shredder::new(1, 0, 1, 0).unwrap();
        let entries = create_ticks(1, 1, Hash::new_unique());
        let (shreds_data, _) = shredder.entries_to_resigned_merkle_shreds_for_tests(
            &leader_keypair,
            &entries,
            true, // is_last_in_slot
            Hash::new_unique(),
            0,
            0,
        );
        let shred = shreds_data.into_iter().next().unwrap();
        let mut other_shred = shred.clone().into_payload();
        let mut shred = shred.into_payload();

        shred::wire::set_retransmitter_signature(shred.as_mut().as_mut(), &Signature::new_unique())
            .unwrap();
        shred::wire::set_retransmitter_signature(
            other_shred.as_mut().as_mut(),
            &Signature::new_unique(),
        )
        .unwrap();

        let shred_packet = shred.to_bytes_packet(None);
        let other_packet = other_shred.to_bytes_packet(None);
        assert_ne!(shred_packet.data(..), other_packet.data(..));

        let deduper = Deduper::<2, [u8]>::new(&mut rng, /*num_bits:*/ 1024);
        let shred_bytes =
            shred::wire::get_shred_without_retransmitter_signature(&shred_packet).unwrap();
        let other_bytes =
            shred::wire::get_shred_without_retransmitter_signature(&other_packet).unwrap();
        assert_eq!(shred_bytes, other_bytes);
        assert!(!deduper.dedup(shred_bytes));
        assert!(deduper.dedup(other_bytes));
    }

    #[test]
    #[should_panic(expected = "shred sigverify worker panicked")]
    fn test_sigverify_worker_panic_is_propagated() {
        let leader_keypair = Arc::new(Keypair::new());
        let node_pubkey = Pubkey::new_unique();
        let leader_pubkey = leader_keypair.pubkey();

        let bank = Bank::new_for_tests(
            &create_genesis_config_with_leader(100, &leader_pubkey, 10).genesis_config,
        );
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let bank_forks = BankForks::new_rw_arc(bank);

        let sigverify_cache = Arc::new(RwLock::new(LruCache::new(
            TEST_SIGVERIFY_LRU_CACHE_CAPACITY,
        )));
        let sigverify_cache_to_poison = sigverify_cache.clone();

        let poison_result = std::thread::spawn(move || {
            let _guard = sigverify_cache_to_poison.write().unwrap();
            panic!("poison shred sigverify cache");
        })
        .join();

        assert!(poison_result.is_err());

        let workers =
            new_sigverify_workers_with_cache(bank_forks, leader_schedule_cache, sigverify_cache, 1);

        let entries = create_ticks(1, 1, Hash::new_unique());
        let shredder = Shredder::new(1, 0, 1, 0).unwrap();

        let (shreds, _) = shredder.entries_to_merkle_shreds_for_tests(
            &leader_keypair,
            &entries,
            false,
            Hash::new_unique(),
            0,
            0,
            &mut ProcessShredsStats::default(),
        );

        let shred = &shreds[0];
        let batch = BytesPacketBatch::from(vec![shred.payload().to_bytes_packet(None)]);
        let mut batches = vec![PacketBatch::from(batch)];

        workers.process_batches(&mut batches, &node_pubkey);
    }

    #[test]
    fn test_sigverify_shreds_verify_batches() {
        let leader_keypair = Arc::new(Keypair::new());
        let wrong_keypair = Keypair::new();
        let node_pubkey = Pubkey::new_unique();
        let leader_pubkey = leader_keypair.pubkey();

        let bank = Bank::new_for_tests(
            &create_genesis_config_with_leader(100, &leader_pubkey, 10).genesis_config,
        );
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let bank_forks = BankForks::new_rw_arc(bank);

        let workers = new_sigverify_workers(bank_forks, leader_schedule_cache, 3);

        let entries = create_ticks(1, 1, Hash::new_unique());
        let shredder = Shredder::new(1, 0, 1, 0).unwrap();

        let (valid_shreds, _) = shredder.entries_to_merkle_shreds_for_tests(
            &leader_keypair,
            &entries,
            false,
            Hash::new_unique(),
            0,
            0,
            &mut ProcessShredsStats::default(),
        );

        let (invalid_shreds, _) = shredder.entries_to_merkle_shreds_for_tests(
            &wrong_keypair,
            &entries,
            false,
            Hash::new_unique(),
            0,
            0,
            &mut ProcessShredsStats::default(),
        );

        let batch = BytesPacketBatch::from(vec![
            valid_shreds[0].payload().to_bytes_packet(None),
            invalid_shreds[0].payload().to_bytes_packet(None),
        ]);
        let mut batches = vec![PacketBatch::from(batch)];

        let worker_stats = workers.process_batches(&mut batches, &node_pubkey);

        assert_eq!(worker_stats.num_discards_post, 1);
        assert!(!batches[0].first().unwrap().meta().discard());
        assert!(batches[0].get(1).unwrap().meta().discard());
    }

    #[test_case(true)]
    #[test_case(false)]
    fn test_maybe_clear_retransmitter_signature(is_last_in_slot: bool) {
        let mut rng = rand::rng();

        let leader_keypair = Keypair::new();
        let chained_merkle_root = Hash::new_from_array(rng.random());

        let shredder = Shredder::new(0, 0, 0, 0).unwrap();
        let entries = vec![Entry::new(&Hash::default(), 0, vec![])];
        let (data_shreds, coding_shreds) = shredder.entries_to_resigned_merkle_shreds_for_tests(
            &leader_keypair,
            &entries,
            is_last_in_slot,
            chained_merkle_root,
            0,
            0,
        );
        let shreds: Vec<_> = data_shreds.into_iter().chain(coding_shreds).collect();

        let upstream_signature = Signature::from([1u8; 64]);
        for shred in shreds {
            let mut payload = shred.into_payload();

            if is_last_in_slot {
                set_retransmitter_signature(payload.as_mut().as_mut(), &upstream_signature)
                    .expect("last FEC set in slot should be of resigned variant");
            }

            let original = payload.clone();
            maybe_clear_retransmitter_signature(&mut payload)
                .expect("valid shred should not be rejected");

            if is_last_in_slot {
                assert_eq!(
                    get_retransmitter_signature(payload.as_ref())
                        .expect("resigned variant should have retransmitter signature"),
                    Signature::default(),
                    "retransmitter signature should be zeroed"
                );
            } else {
                assert_eq!(payload, original, "shred should not be modified");
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
            &mut ProcessShredsStats::default(),
        );

        let bytes_packet = shreds[0].payload().to_bytes_packet(None);
        let buffer_ptr = bytes_packet.buffer().as_ptr();

        let mut stats = ShredSigVerifyStats::new(Instant::now());
        let repair_nonce_location_lookup = |_| None;

        let (payload, location) =
            extract_shred_and_location(&bytes_packet, &repair_nonce_location_lookup, &mut stats)
                .unwrap();

        assert!(location.is_none());
        assert_eq!(payload.bytes.as_ptr(), buffer_ptr);
        assert_eq!(payload.bytes.as_ref(), shreds[0].payload().bytes.as_ref());
    }

    #[test]
    fn test_sigverify_workers_fanout_aggregates_stats() {
        const NUM_WORKERS: usize = 3;
        const NUM_BATCHES: usize = NUM_WORKERS;

        let leader_keypair = Arc::new(Keypair::new());
        let wrong_keypair = Keypair::new();
        let node_pubkey = Pubkey::new_unique();
        let leader_pubkey = leader_keypair.pubkey();

        let bank = Bank::new_for_tests(
            &create_genesis_config_with_leader(100, &leader_pubkey, 10).genesis_config,
        );
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let bank_forks = BankForks::new_rw_arc(bank);

        let workers = new_sigverify_workers(bank_forks, leader_schedule_cache, NUM_WORKERS);

        let entries = create_ticks(1, 1, Hash::new_unique());
        let shredder = Shredder::new(1, 0, 1, 0).unwrap();

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
                &mut ProcessShredsStats::default(),
            );

            let shred = &shreds[0];
            let batch = BytesPacketBatch::from(vec![shred.payload().to_bytes_packet(None)]);
            batches.push(PacketBatch::from(batch));
        }

        let worker_stats = workers.process_batches(&mut batches, &node_pubkey);

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
        const NUM_WORKERS: usize = 4;
        const NUM_BATCHES: usize = NUM_WORKERS;

        let leader_keypair = Arc::new(Keypair::new());
        let node_pubkey = Pubkey::new_unique();
        let leader_pubkey = leader_keypair.pubkey();

        let bank = Bank::new_for_tests(
            &create_genesis_config_with_leader(100, &leader_pubkey, 10).genesis_config,
        );
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let bank_forks = BankForks::new_rw_arc(bank);

        let workers = new_sigverify_workers(bank_forks, leader_schedule_cache, NUM_WORKERS);

        let entries = create_ticks(1, 1, Hash::new_unique());
        let shredder = Shredder::new(1, 0, 1, 0).unwrap();

        let (shreds, _) = shredder.entries_to_merkle_shreds_for_tests(
            &leader_keypair,
            &entries,
            false,
            Hash::new_unique(),
            0,
            0,
            &mut ProcessShredsStats::default(),
        );

        let shred = &shreds[0];
        let mut batches = Vec::with_capacity(NUM_BATCHES);

        for _ in 0..NUM_BATCHES {
            let batch = BytesPacketBatch::from(vec![shred.payload().to_bytes_packet(None)]);
            batches.push(PacketBatch::from(batch));
        }

        let worker_stats = workers.process_batches(&mut batches, &node_pubkey);

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
        let node_pubkey = Pubkey::new_unique();
        let leader_pubkey = leader_keypair.pubkey();

        let bank = Bank::new_for_tests(
            &create_genesis_config_with_leader(100, &leader_pubkey, 10).genesis_config,
        );
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let bank_forks = BankForks::new_rw_arc(bank);

        let workers = new_sigverify_workers(bank_forks, leader_schedule_cache, 3);

        let entries = create_ticks(1, 1, Hash::new_unique());
        let shredder = Shredder::new(1, 0, 1, 0).unwrap();

        let (shreds_a, _) = shredder.entries_to_merkle_shreds_for_tests(
            &leader_keypair,
            &entries,
            false,
            Hash::new_unique(),
            0,
            0,
            &mut ProcessShredsStats::default(),
        );

        let (shreds_b, _) = shredder.entries_to_merkle_shreds_for_tests(
            &leader_keypair,
            &entries,
            false,
            Hash::new_unique(),
            0,
            0,
            &mut ProcessShredsStats::default(),
        );

        let shred_a = &shreds_a[0];
        let shred_b = &shreds_b[0];

        let turbine_a = shred_a.payload().to_bytes_packet(None);

        let mut repair_a = shred_a.payload().to_bytes_packet(None);
        repair_a.meta_mut().flags |= PacketFlags::REPAIR;

        let mut repair_b = shred_b.payload().to_bytes_packet(None);
        repair_b.meta_mut().flags |= PacketFlags::REPAIR;

        let turbine_b = shred_b.payload().to_bytes_packet(None);

        let batch = BytesPacketBatch::from(vec![turbine_a, repair_a, repair_b, turbine_b]);
        let mut batches = vec![PacketBatch::from(batch)];

        let worker_stats = workers.process_batches(&mut batches, &node_pubkey);
        let batch = &batches[0];

        assert!(!batch.first().unwrap().meta().discard());
        assert!(!batch.get(1).unwrap().meta().discard());
        assert!(!batch.get(2).unwrap().meta().discard());
        assert!(batch.get(3).unwrap().meta().discard());

        assert_eq!(worker_stats.num_duplicates, 1);
        assert_eq!(worker_stats.num_discards_post, 1);
    }
}
