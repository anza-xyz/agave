//! `window_service` handles the data plane incoming shreds, storing them in
//!   blockstore and retransmitting where required
//!

use {
    crate::{
        completed_data_sets_service::CompletedDataSetsSender,
        repair::{
            block_id_repair_service::{BlockIdRepairChannels, BlockIdRepairService},
            repair_service::{
                OutstandingShredRepairs, RepairInfo, RepairService, RepairServiceChannels,
            },
        },
        result::{Error, Result},
    },
    agave_feature_set as feature_set,
    crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError, unbounded},
    rayon::{ThreadPool, prelude::*},
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::{
        blockstore::{
            Blockstore, BlockstoreInsertionMetrics, PossibleDuplicateShred, RecoveredShredBatch,
            ShredRecoveryTask,
        },
        blockstore_db::{DBPinnableSlice, WriteBatch},
        blockstore_meta::BlockLocation,
        shred::{self, ReedSolomonCache, Shred, filter::ShredRecoveryContext},
    },
    solana_measure::measure::Measure,
    solana_net_utils::PinnedXdpSender,
    solana_rayon_threadlimit::get_thread_count,
    solana_runtime::bank_forks::{BankForks, SharableBanks},
    solana_streamer::{evicting_sender::EvictingSender, streamer::ChannelSend},
    std::{
        borrow::Cow,
        net::UdpSocket,
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

type DuplicateSlotSender = Sender<Slot>;
pub(crate) type DuplicateSlotReceiver = Receiver<Slot>;

#[derive(Default)]
struct WindowServiceMetrics {
    run_insert_count: u64,
    num_repairs: AtomicUsize,
    num_shreds_received: usize,
    handle_packets_elapsed_us: u64,
    shred_receiver_elapsed_us: u64,
    num_errors: u64,
    num_errors_blockstore: u64,
    num_errors_cross_beam_recv_timeout: u64,
    num_errors_other: u64,
    num_errors_try_crossbeam_send: u64,
    num_recovery_candidates: usize,
    num_recovery_candidates_dropped: usize,
}

#[derive(Default)]
struct WindowRecoveryMetrics {
    num_recovery_tasks: usize,
    num_recovered_batches: usize,
    num_recovery_tasks_failed: usize,
    recovery_queue_depth_max: usize,
}

impl WindowServiceMetrics {
    const NAME: &str = "recv-window-insert-shreds";

    fn report_metrics(&self) {
        datapoint_info!(
            Self::NAME,
            (
                "handle_packets_elapsed_us",
                self.handle_packets_elapsed_us,
                i64
            ),
            ("run_insert_count", self.run_insert_count as i64, i64),
            ("num_repairs", self.num_repairs.load(Ordering::Relaxed), i64),
            ("num_shreds_received", self.num_shreds_received, i64),
            (
                "shred_receiver_elapsed_us",
                self.shred_receiver_elapsed_us as i64,
                i64
            ),
            ("num_errors", self.num_errors, i64),
            ("num_errors_blockstore", self.num_errors_blockstore, i64),
            ("num_errors_other", self.num_errors_other, i64),
            (
                "num_errors_try_crossbeam_send",
                self.num_errors_try_crossbeam_send,
                i64
            ),
            (
                "num_errors_cross_beam_recv_timeout",
                self.num_errors_cross_beam_recv_timeout,
                i64
            ),
            ("num_recovery_candidates", self.num_recovery_candidates, i64),
            (
                "num_recovery_candidates_dropped",
                self.num_recovery_candidates_dropped,
                i64
            ),
        );
    }

    fn record_error(&mut self, err: &Error) {
        self.num_errors += 1;
        match err {
            Error::TrySend => self.num_errors_try_crossbeam_send += 1,
            Error::RecvTimeout(_) => self.num_errors_cross_beam_recv_timeout += 1,
            Error::Blockstore(err) => {
                self.num_errors_blockstore += 1;
                error!("blockstore error: {err}");
            }
            _ => self.num_errors_other += 1,
        }
    }
}

impl WindowRecoveryMetrics {
    const NAME: &str = "recv-window-recovery-shreds";

    fn record_task_batch(&mut self, num_tasks: usize, queue_depth: usize) {
        self.num_recovery_tasks += num_tasks;
        self.recovery_queue_depth_max = self.recovery_queue_depth_max.max(queue_depth);
    }

    fn report_metrics(&self) {
        datapoint_info!(
            Self::NAME,
            ("num_recovery_tasks", self.num_recovery_tasks, i64),
            ("num_recovered_batches", self.num_recovered_batches, i64),
            (
                "num_recovery_tasks_failed",
                self.num_recovery_tasks_failed,
                i64
            ),
            (
                "recovery_queue_depth_max",
                self.recovery_queue_depth_max,
                i64
            ),
        );
    }
}

/// Per-shred duplicate handling, extracted from `run_check_duplicate` so callers
/// can run the same duplicate-detection path without gossip/channel side effects.
///
/// Returns the duplicate proof (`shred` and the conflicting payload) when one is
/// detected, leaving propagation to the caller.
pub fn check_duplicate_shred(
    blockstore: &Blockstore,
    shred: PossibleDuplicateShred,
    no_verify_chained_merkle_root: bool,
) -> Result<Option<(Shred, shred::Payload)>> {
    let shred_slot = shred.slot();
    let (shred1, shred2) = match shred {
        PossibleDuplicateShred::LastIndexConflict(shred, conflict)
        | PossibleDuplicateShred::ErasureConflict(shred, conflict)
        | PossibleDuplicateShred::MerkleRootConflict(shred, conflict) => (shred, conflict),
        PossibleDuplicateShred::FixedFECChainedMerkleRootConflict(_slot) => {
            if no_verify_chained_merkle_root {
                // If we're in the full alpenglow epoch, we stop validating the chained merkle root.
                // In Alpenglow we only use the double merkle root
                return Ok(None);
            }
            blockstore.set_dead_slot(shred_slot)?;
            return Ok(None);
        }
        PossibleDuplicateShred::Exists(shred) => {
            // Unlike the other cases we have to wait until here to decide to handle the duplicate and store
            // in blockstore. This is because the duplicate could have been part of the same insert batch,
            // so we wait until the batch has been written.
            if blockstore.has_duplicate_shreds_in_slot(shred_slot) {
                return Ok(None); // A duplicate is already recorded
            }
            let Some(existing_shred_payload) = blockstore.is_shred_duplicate(&shred) else {
                return Ok(None); // Not a duplicate
            };
            blockstore.store_duplicate_slot(
                shred_slot,
                existing_shred_payload.clone(),
                shred.clone().into_payload(),
            )?;
            (shred, shred::Payload::from(existing_shred_payload))
        }
    };

    Ok(Some((shred1, shred2)))
}

fn run_check_duplicate(
    cluster_info: &ClusterInfo,
    blockstore: &Blockstore,
    shred_receiver: &Receiver<PossibleDuplicateShred>,
    duplicate_slots_sender: &DuplicateSlotSender,
    bank_forks: &RwLock<BankForks>,
) -> Result<()> {
    let (mut root_bank, migration_status) = {
        let bank_forks_r = bank_forks.read().unwrap();
        (bank_forks_r.root_bank(), bank_forks_r.migration_status())
    };
    let mut last_updated = Instant::now();
    let check_duplicate = |shred: PossibleDuplicateShred| -> Result<()> {
        if last_updated.elapsed().as_nanos() > root_bank.ns_per_slot {
            // Grabs bank forks lock once a slot
            last_updated = Instant::now();
            root_bank = bank_forks.read().unwrap().root_bank();
        }
        let shred_slot = shred.slot();
        let no_verify_chained_merkle_root = shred::filter::check_feature_activation_from_bank(
            &feature_set::alpenglow::id(),
            shred_slot,
            &root_bank,
        );

        let duplicate = check_duplicate_shred(blockstore, shred, no_verify_chained_merkle_root)?;

        let should_mark_dead = migration_status
            .genesis_block()
            .is_some_and(|genesis| shred_slot > genesis.slot);
        if should_mark_dead {
            // Apart from Exists all existing cases mark dead inline in blockstore.
            // Once Alpenglow is active we can fully remove this thread and move Exists to inline as well.
            blockstore.set_dead_slot_if_duplicate_and_not_full(shred_slot)?;
        }

        let Some((shred1, shred2)) = duplicate else {
            return Ok(());
        };

        if migration_status.should_respond_to_ancestor_hashes_requests(shred_slot) {
            // In alpenglow we store the duplicate block proofs in blockstore for the purposes of slashing,
            // however we do not need to propagate the duplicate proof through gossip.
            // We still propagate during the mixed migration epoch, to account for other nodes that are stuck
            // and require a duplicate proof to proceed
            cluster_info.push_duplicate_shred(&shred1, &shred2)?;
        }

        if !migration_status.is_alpenglow_enabled() {
            // The state machine can be exited as soon as alpenglow is enabled.
            // Notify duplicate consensus state machine. If channel is full we wait.
            duplicate_slots_sender.send(shred_slot)?;
        }

        Ok(())
    };
    const RECV_TIMEOUT: Duration = Duration::from_millis(200);
    std::iter::once(shred_receiver.recv_timeout(RECV_TIMEOUT)?)
        .chain(shred_receiver.try_iter())
        .try_for_each(check_duplicate)
}

#[allow(clippy::too_many_arguments)]
fn run_insert<'db, F>(
    thread_pool: &ThreadPool,
    verified_receiver: &Receiver<Vec<(shred::Payload, /*is_repaired:*/ bool, BlockLocation)>>,
    recovery_sender: &EvictingSender<Vec<ShredRecoveryTask>>,
    blockstore: &'db Blockstore,
    pinnable_slice: &mut DBPinnableSlice<'db>,
    write_batch: &mut WriteBatch,
    handle_duplicate: F,
    metrics: &mut BlockstoreInsertionMetrics,
    ws_metrics: &mut WindowServiceMetrics,
    completed_data_sets_sender: Option<&CompletedDataSetsSender>,
) -> Result<()>
where
    F: Fn(PossibleDuplicateShred),
{
    const RECV_TIMEOUT: Duration = Duration::from_millis(200);
    let mut shred_receiver_elapsed = Measure::start("shred_receiver_elapsed");
    let mut shreds = verified_receiver.recv_timeout(RECV_TIMEOUT)?;
    shreds.extend(verified_receiver.try_iter().flatten());
    shred_receiver_elapsed.stop();
    ws_metrics.shred_receiver_elapsed_us += shred_receiver_elapsed.as_us();
    ws_metrics.run_insert_count += 1;
    let handle_shred = |(shred, repair, block_location): (shred::Payload, bool, BlockLocation)| {
        if repair {
            ws_metrics.num_repairs.fetch_add(1, Ordering::Relaxed);
        }
        let shred = Shred::new_from_serialized_shred(shred).ok()?;
        Some((Cow::Owned(shred), repair, block_location))
    };
    let now = Instant::now();
    let shreds: Vec<_> = thread_pool.install(|| {
        shreds
            .into_par_iter()
            .with_min_len(32)
            .filter_map(handle_shred)
            .collect()
    });
    ws_metrics.handle_packets_elapsed_us += now.elapsed().as_micros() as u64;
    ws_metrics.num_shreds_received += shreds.len();
    let (completed_data_sets, recovery_tasks) = blockstore
        .insert_shreds_at_location_prepare_recovery(
            shreds,
            false, // is_trusted
            pinnable_slice,
            write_batch,
            &handle_duplicate,
            metrics,
        )?;

    ws_metrics.num_recovery_candidates += recovery_tasks.len();
    if !recovery_tasks.is_empty() {
        match recovery_sender.try_send(recovery_tasks) {
            Ok(()) => {}
            Err(TrySendError::Full(candidates)) => {
                ws_metrics.num_recovery_candidates_dropped += candidates.len();
            }
            Err(TrySendError::Disconnected(_)) => return Err(Error::TrySend),
        }
    }

    if let Some(sender) = completed_data_sets_sender {
        sender.try_send(completed_data_sets)?;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_recovery<'db, F>(
    recovery_receiver: &Receiver<Vec<ShredRecoveryTask>>,
    blockstore: &'db Blockstore,
    shred_recovery_context: &mut ShredRecoveryContext,
    recovered_batch_scratch: &mut Vec<RecoveredShredBatch>,
    pinnable_slice: &mut DBPinnableSlice<'db>,
    write_batch: &mut WriteBatch,
    handle_duplicate: F,
    metrics: &mut BlockstoreInsertionMetrics,
    recovery_metrics: &mut WindowRecoveryMetrics,
    completed_data_sets_sender: Option<&CompletedDataSetsSender>,
) -> Result<()>
where
    F: Fn(PossibleDuplicateShred),
{
    const RECV_TIMEOUT: Duration = Duration::from_millis(200);
    let recovery_tasks = recovery_receiver.recv_timeout(RECV_TIMEOUT)?;
    recovery_metrics.record_task_batch(recovery_tasks.len(), recovery_receiver.len());
    let mut next_recovered_batch = 0;
    let mut recovery_elapsed = Measure::start("Shred recovery");
    for task in recovery_tasks {
        let erasure_set = task.erasure_set();
        // Successful batches occupy a compact prefix of the scratch buffer. A
        // failed recovery leaves the index unchanged, so the next task reuses
        // the same allocation.
        if next_recovered_batch == recovered_batch_scratch.len() {
            recovered_batch_scratch.push(RecoveredShredBatch::new(erasure_set));
        }
        if blockstore.recover_shreds_from_task(
            task,
            shred_recovery_context,
            &mut recovered_batch_scratch[next_recovered_batch],
        ) {
            next_recovered_batch += 1;
            recovery_metrics.num_recovered_batches += 1;
        } else {
            recovery_metrics.num_recovery_tasks_failed += 1;
        }
    }
    recovery_elapsed.stop();
    metrics.shred_recovery_elapsed_us += recovery_elapsed.as_us();

    let recovered_batches = &mut recovered_batch_scratch[..next_recovered_batch];
    if recovered_batches.is_empty() {
        return Ok(());
    }
    let completed_data_sets = blockstore.insert_recovered_shreds(
        recovered_batches,
        shred_recovery_context,
        pinnable_slice,
        write_batch,
        &handle_duplicate,
        metrics,
    )?;
    if !completed_data_sets.is_empty()
        && let Some(sender) = completed_data_sets_sender
    {
        sender.try_send(completed_data_sets)?;
    }
    Ok(())
}

pub struct WindowServiceChannels {
    pub verified_receiver: Receiver<Vec<(shred::Payload, /*is_repaired:*/ bool, BlockLocation)>>,
    pub retransmit_sender: EvictingSender<Vec<shred::Payload>>,
    pub completed_data_sets_sender: Option<CompletedDataSetsSender>,
    pub duplicate_slots_sender: DuplicateSlotSender,
    pub repair_service_channels: RepairServiceChannels,
    pub block_id_repair_channels: BlockIdRepairChannels,
}

impl WindowServiceChannels {
    pub fn new(
        verified_receiver: Receiver<Vec<(shred::Payload, /*is_repaired:*/ bool, BlockLocation)>>,
        retransmit_sender: EvictingSender<Vec<shred::Payload>>,
        completed_data_sets_sender: Option<CompletedDataSetsSender>,
        duplicate_slots_sender: DuplicateSlotSender,
        repair_service_channels: RepairServiceChannels,
        block_id_repair_channels: BlockIdRepairChannels,
    ) -> Self {
        Self {
            verified_receiver,
            retransmit_sender,
            completed_data_sets_sender,
            duplicate_slots_sender,
            repair_service_channels,
            block_id_repair_channels,
        }
    }
}

pub(crate) struct WindowService {
    t_insert: JoinHandle<()>,
    t_recovery: JoinHandle<()>,
    t_check_duplicate: JoinHandle<()>,
    repair_service: RepairService,
    block_id_repair_service: BlockIdRepairService,
}

impl WindowService {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        blockstore: Arc<Blockstore>,
        repair_socket: Arc<UdpSocket>,
        ancestor_hashes_socket: Arc<UdpSocket>,
        block_id_repair_socket: Arc<UdpSocket>,
        exit: Arc<AtomicBool>,
        repair_info: RepairInfo,
        window_service_channels: WindowServiceChannels,
        shred_version: u16,
        outstanding_repair_requests: Arc<RwLock<OutstandingShredRepairs>>,
        repair_xdp_sender: Option<PinnedXdpSender>,
    ) -> WindowService {
        let cluster_info = repair_info.cluster_info.clone();
        let bank_forks = repair_info.bank_forks.clone();

        let WindowServiceChannels {
            verified_receiver,
            retransmit_sender,
            completed_data_sets_sender,
            duplicate_slots_sender,
            repair_service_channels,
            block_id_repair_channels,
        } = window_service_channels;

        let repair_service = RepairService::new(
            blockstore.clone(),
            exit.clone(),
            repair_socket.clone(),
            ancestor_hashes_socket,
            repair_info.clone(),
            outstanding_repair_requests.clone(),
            repair_service_channels,
            repair_xdp_sender,
        );

        let block_id_repair_service = BlockIdRepairService::new(
            exit.clone(),
            blockstore.clone(),
            block_id_repair_socket,
            repair_socket,
            block_id_repair_channels,
            repair_info,
            outstanding_repair_requests,
        );

        let (duplicate_sender, duplicate_receiver) = unbounded();

        let t_check_duplicate = Self::start_check_duplicate_thread(
            cluster_info,
            exit.clone(),
            blockstore.clone(),
            duplicate_receiver,
            duplicate_slots_sender,
            bank_forks.clone(),
        );

        const RECOVERY_CHANNEL_CAPACITY: usize = 64;
        let (recovery_sender, recovery_receiver) =
            EvictingSender::new_bounded(RECOVERY_CHANNEL_CAPACITY);
        let sharable_banks = bank_forks.read().unwrap().sharable_banks();
        let t_recovery = Self::start_window_recovery_thread(
            exit.clone(),
            blockstore.clone(),
            sharable_banks,
            shred_version,
            recovery_receiver,
            duplicate_sender.clone(),
            completed_data_sets_sender.clone(),
            retransmit_sender,
        );
        let t_insert = Self::start_window_insert_thread(
            exit,
            blockstore,
            verified_receiver,
            recovery_sender,
            duplicate_sender,
            completed_data_sets_sender,
        );

        WindowService {
            t_insert,
            t_recovery,
            t_check_duplicate,
            repair_service,
            block_id_repair_service,
        }
    }

    fn start_check_duplicate_thread(
        cluster_info: Arc<ClusterInfo>,
        exit: Arc<AtomicBool>,
        blockstore: Arc<Blockstore>,
        duplicate_receiver: Receiver<PossibleDuplicateShred>,
        duplicate_slots_sender: DuplicateSlotSender,
        bank_forks: Arc<RwLock<BankForks>>,
    ) -> JoinHandle<()> {
        Builder::new()
            .name("solWinCheckDup".to_string())
            .spawn(move || {
                while !exit.load(Ordering::Relaxed) {
                    if let Err(e) = run_check_duplicate(
                        &cluster_info,
                        &blockstore,
                        &duplicate_receiver,
                        &duplicate_slots_sender,
                        &bank_forks,
                    ) && Self::should_exit_on_error(e)
                    {
                        break;
                    }
                }
            })
            .unwrap()
    }

    fn start_window_insert_thread(
        exit: Arc<AtomicBool>,
        blockstore: Arc<Blockstore>,
        verified_receiver: Receiver<Vec<(shred::Payload, /*is_repaired:*/ bool, BlockLocation)>>,
        recovery_sender: EvictingSender<Vec<ShredRecoveryTask>>,
        check_duplicate_sender: Sender<PossibleDuplicateShred>,
        completed_data_sets_sender: Option<CompletedDataSetsSender>,
    ) -> JoinHandle<()> {
        Builder::new()
            .name("solWinInsert".to_string())
            .spawn(move || {
                let thread_pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(get_thread_count().min(8))
                    // Use the current thread as one of the workers. This reduces overhead when the
                    // pool is used to process a small number of shreds, since they'll be processed
                    // directly on the current thread.
                    .use_current_thread()
                    .thread_name(|i| format!("solWinInsert{i:02}"))
                    .build()
                    .unwrap();
                let handle_duplicate = |possible_duplicate_shred| {
                    let _ = check_duplicate_sender.send(possible_duplicate_shred);
                };

                const METRICS_REPORTING_INTERVAL: Duration = Duration::from_secs(2);
                let mut metrics = BlockstoreInsertionMetrics::default();
                let mut ws_metrics = WindowServiceMetrics::default();
                let mut last_print = Instant::now();
                let mut pinnable_slice = blockstore.new_pinnable_slice();
                let mut write_batch = blockstore.get_write_batch();

                while !exit.load(Ordering::Relaxed) {
                    if let Err(e) = run_insert(
                        &thread_pool,
                        &verified_receiver,
                        &recovery_sender,
                        &blockstore,
                        &mut pinnable_slice,
                        &mut write_batch,
                        handle_duplicate,
                        &mut metrics,
                        &mut ws_metrics,
                        completed_data_sets_sender.as_ref(),
                    ) {
                        ws_metrics.record_error(&e);
                        if Self::should_exit_on_error(e) {
                            break;
                        }
                    }

                    if last_print.elapsed() > METRICS_REPORTING_INTERVAL {
                        metrics.report_metrics("solWinInsert");
                        metrics = BlockstoreInsertionMetrics::default();
                        ws_metrics.report_metrics();
                        ws_metrics = WindowServiceMetrics::default();
                        last_print = Instant::now();
                    }
                }
            })
            .unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    fn start_window_recovery_thread(
        exit: Arc<AtomicBool>,
        blockstore: Arc<Blockstore>,
        sharable_banks: SharableBanks,
        shred_version: u16,
        recovery_receiver: Receiver<Vec<ShredRecoveryTask>>,
        check_duplicate_sender: Sender<PossibleDuplicateShred>,
        completed_data_sets_sender: Option<CompletedDataSetsSender>,
        retransmit_sender: EvictingSender<Vec<shred::Payload>>,
    ) -> JoinHandle<()> {
        Builder::new()
            .name("solWinRecover".to_string())
            .spawn(move || {
                let handle_duplicate = |possible_duplicate_shred| {
                    let _ = check_duplicate_sender.send(possible_duplicate_shred);
                };
                let mut shred_recovery_context = ShredRecoveryContext::new(
                    ReedSolomonCache::default(),
                    retransmit_sender,
                    sharable_banks.root(),
                    shred_version,
                );
                let mut pinnable_slice = blockstore.new_pinnable_slice();
                let mut write_batch = blockstore.get_write_batch();
                let mut metrics = BlockstoreInsertionMetrics::default();
                let mut recovery_metrics = WindowRecoveryMetrics::default();
                let mut recovered_batch_scratch = Vec::new();
                let mut last_print = Instant::now();
                const METRICS_REPORTING_INTERVAL: Duration = Duration::from_secs(2);

                while !exit.load(Ordering::Relaxed) {
                    shred_recovery_context.maybe_update(sharable_banks.root());
                    if let Err(e) = run_recovery(
                        &recovery_receiver,
                        &blockstore,
                        &mut shred_recovery_context,
                        &mut recovered_batch_scratch,
                        &mut pinnable_slice,
                        &mut write_batch,
                        handle_duplicate,
                        &mut metrics,
                        &mut recovery_metrics,
                        completed_data_sets_sender.as_ref(),
                    ) && Self::should_exit_on_error(e)
                    {
                        break;
                    }
                    if last_print.elapsed() > METRICS_REPORTING_INTERVAL {
                        metrics.report_metrics("solWinRecover");
                        metrics = BlockstoreInsertionMetrics::default();
                        recovery_metrics.report_metrics();
                        recovery_metrics = WindowRecoveryMetrics::default();
                        last_print = Instant::now();
                    }
                    shred_recovery_context.maybe_submit_stats();
                }
            })
            .unwrap()
    }

    fn should_exit_on_error(e: Error) -> bool {
        match e {
            Error::RecvTimeout(RecvTimeoutError::Disconnected) => true,
            Error::RecvTimeout(RecvTimeoutError::Timeout) => false,
            Error::Send => true,
            _ => {
                let version = solana_version::version!();
                datapoint_error!(
                    "error",
                    ("thread", thread::current().name().unwrap_or("?"), String),
                    ("message", format!("{e}"), String),
                    ("version", version, String)
                );
                error!("thread {:?} error {:?}", thread::current().name(), e);
                false
            }
        }
    }

    pub(crate) fn join(self) -> thread::Result<()> {
        self.t_insert.join()?;
        self.t_recovery.join()?;
        self.t_check_duplicate.join()?;
        self.repair_service.join()?;
        self.block_id_repair_service.join()
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crossbeam_channel::bounded,
        rand::Rng,
        solana_entry::entry::{Entry, create_ticks},
        solana_gossip::contact_info::ContactInfo,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_ledger::{
            blockstore::{Blockstore, make_many_slot_entries},
            genesis_utils::create_genesis_config,
            get_tmp_ledger_path_auto_delete,
            shred::{ProcessShredsStats, Shredder},
        },
        solana_net_utils::SocketAddrSpace,
        solana_runtime::bank::Bank,
        solana_signer::Signer,
        solana_time_utils::timestamp,
    };

    fn local_entries_to_shred(
        entries: &[Entry],
        slot: Slot,
        parent: Slot,
        keypair: &Keypair,
    ) -> Vec<Shred> {
        let shredder = Shredder::new(slot, parent, 0, 0).unwrap();
        let (data_shreds, _) = shredder.entries_to_merkle_shreds_for_tests(
            keypair,
            entries,
            true, // is_last_in_slot
            // chained_merkle_root
            Hash::new_from_array(rand::rng().random()),
            0, // next_shred_index
            0, // next_code_index
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        );
        data_shreds
    }

    #[test]
    fn test_process_shred() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let num_entries = 10;
        let original_entries = create_ticks(num_entries, 0, Hash::default());
        let mut shreds = local_entries_to_shred(&original_entries, 0, 0, &Keypair::new());
        shreds.reverse();
        blockstore
            .insert_shreds(shreds, false)
            .expect("Expect successful processing of shred");

        assert_eq!(blockstore.get_slot_entries(0, 0).unwrap(), original_entries);
    }

    #[test]
    fn test_run_check_duplicate() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let genesis_config = create_genesis_config(10_000).genesis_config;
        let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis_config));
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let (sender, receiver) = bounded(1024);
        let (duplicate_slot_sender, duplicate_slot_receiver) = bounded(1024);
        let (shreds, _) = make_many_slot_entries(5, 5, 10);
        blockstore.insert_shreds(shreds.clone(), false).unwrap();
        let duplicate_index = 0;
        let original_shred = shreds[duplicate_index].clone();
        let duplicate_shred = {
            let (mut shreds, _) = make_many_slot_entries(5, 1, 10);
            shreds.swap_remove(duplicate_index)
        };
        assert_eq!(duplicate_shred.slot(), shreds[0].slot());
        let duplicate_shred_slot = duplicate_shred.slot();
        sender
            .send(PossibleDuplicateShred::Exists(duplicate_shred.clone()))
            .unwrap();
        assert!(!blockstore.has_duplicate_shreds_in_slot(duplicate_shred_slot));
        let keypair = Keypair::new();
        let contact_info = ContactInfo::new_localhost(&keypair.pubkey(), timestamp());
        let cluster_info = ClusterInfo::new(
            contact_info,
            Arc::new(keypair),
            SocketAddrSpace::Unspecified,
        );
        run_check_duplicate(
            &cluster_info,
            &blockstore,
            &receiver,
            &duplicate_slot_sender,
            &bank_forks,
        )
        .unwrap();

        // Make sure the correct duplicate proof was stored
        let duplicate_proof = blockstore.get_duplicate_slot(duplicate_shred_slot).unwrap();
        assert_eq!(duplicate_proof.shred1, *original_shred.payload());
        assert_eq!(duplicate_proof.shred2, *duplicate_shred.payload());

        // Make sure a duplicate signal was sent
        assert_eq!(
            duplicate_slot_receiver.try_recv().unwrap(),
            duplicate_shred_slot
        );
    }

    #[test]
    fn test_check_duplicate_shred_returns_duplicate_proof() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();
        let (shreds, _) = make_many_slot_entries(5, 5, 10);
        blockstore.insert_shreds(shreds.clone(), false).unwrap();
        let duplicate_index = 0;
        let original_shred = shreds[duplicate_index].clone();
        let duplicate_shred = {
            let (mut shreds, _) = make_many_slot_entries(5, 1, 10);
            shreds.swap_remove(duplicate_index)
        };
        let duplicate_shred_slot = duplicate_shred.slot();
        assert!(!blockstore.has_duplicate_shreds_in_slot(duplicate_shred_slot));

        let (returned_shred, conflicting_payload) = check_duplicate_shred(
            &blockstore,
            PossibleDuplicateShred::Exists(duplicate_shred.clone()),
            true,
        )
        .unwrap()
        .unwrap();

        assert_eq!(returned_shred.payload(), duplicate_shred.payload());
        assert_eq!(conflicting_payload, *original_shred.payload());
        let duplicate_proof = blockstore.get_duplicate_slot(duplicate_shred_slot).unwrap();
        assert_eq!(duplicate_proof.shred1, *original_shred.payload());
        assert_eq!(duplicate_proof.shred2, *duplicate_shred.payload());
    }

    #[test]
    fn test_store_duplicate_shreds_same_batch() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let (duplicate_shred_sender, duplicate_shred_receiver) = bounded(1024);
        let (duplicate_slot_sender, duplicate_slot_receiver) = bounded(1024);
        let exit = Arc::new(AtomicBool::new(false));
        let keypair = Keypair::new();
        let contact_info = ContactInfo::new_localhost(&keypair.pubkey(), timestamp());
        let cluster_info = Arc::new(ClusterInfo::new(
            contact_info,
            Arc::new(keypair),
            SocketAddrSpace::Unspecified,
        ));
        let genesis_config = create_genesis_config(10_000).genesis_config;
        let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis_config));

        // Start duplicate thread receiving and inserting duplicates
        let t_check_duplicate = WindowService::start_check_duplicate_thread(
            cluster_info,
            exit.clone(),
            blockstore.clone(),
            duplicate_shred_receiver,
            duplicate_slot_sender,
            bank_forks.clone(),
        );

        let handle_duplicate = |shred| {
            let _ = duplicate_shred_sender.send(shred);
        };
        let mut pinnable_slice = blockstore.new_pinnable_slice();
        let mut write_batch = blockstore.get_write_batch();
        let num_trials = 100;
        for slot in 0..num_trials {
            let (shreds, _) = make_many_slot_entries(slot, 1, 10);
            let duplicate_index = 0;
            let original_shred = shreds[duplicate_index].clone();
            let duplicate_shred = {
                let (mut shreds, _) = make_many_slot_entries(slot, 1, 10);
                shreds.swap_remove(duplicate_index)
            };
            assert_eq!(duplicate_shred.slot(), slot);
            // Simulate storing both duplicate shreds in the same batch
            let shreds = [&original_shred, &duplicate_shred]
                .into_iter()
                .map(|shred| {
                    (
                        Cow::Borrowed(shred),
                        /*is_repaired:*/ false,
                        BlockLocation::Original,
                    )
                });
            blockstore
                .insert_shreds_at_location_prepare_recovery(
                    shreds,
                    false, // is_trusted
                    &mut pinnable_slice,
                    &mut write_batch,
                    &handle_duplicate,
                    &mut BlockstoreInsertionMetrics::default(),
                )
                .unwrap();

            // Make sure a duplicate signal was sent
            assert_eq!(
                duplicate_slot_receiver
                    .recv_timeout(Duration::from_millis(5_000))
                    .unwrap(),
                slot
            );

            // Make sure the correct duplicate proof was stored
            let duplicate_proof = blockstore.get_duplicate_slot(slot).unwrap();
            assert_eq!(duplicate_proof.shred1, *original_shred.payload());
            assert_eq!(duplicate_proof.shred2, *duplicate_shred.payload());
        }
        exit.store(true, Ordering::Relaxed);
        t_check_duplicate.join().unwrap();
    }
}
