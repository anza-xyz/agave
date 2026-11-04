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
    crossbeam_channel::{
        Receiver, RecvTimeoutError, Sender, TrySendError, bounded, select, unbounded,
    },
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
        shred::{
            self, ErasureSetId, Shred,
            filter::{RetransmitSender, ShredFilterContext, ShredRecoveryContext},
        },
    },
    solana_measure::measure::Measure,
    solana_net_utils::PinnedXdpSender,
    solana_rayon_threadlimit::get_thread_count,
    solana_runtime::bank_forks::{BankForks, SharableBanks},
    std::{
        borrow::Cow,
        collections::{BTreeMap, BTreeSet},
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

// Each channel message contains one recovered FEC set.
// The channel capacity does not include validation results kept in pending_results.
const RECOVERED_CHANNEL_CAPACITY: usize = 1024;
const MAX_RECOVERED_BATCHES_PER_INSERT: usize = 64;

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
    pending_recovery_tasks: usize,
    pending_recovery_tasks_max: usize,
    pending_validation_reloads: usize,
    pending_validation_reloads_max: usize,
}

#[derive(Default)]
struct WindowRecoveryMetrics {
    shred_recovery_elapsed_us: u64,
    num_recovery_tasks: usize,
    num_recovered_batches: usize,
    num_recovered_batches_dropped: usize,
    num_recovery_tasks_failed: usize,
    recovery_queue_depth_max: usize,
    pending_recovered_batches: usize,
    pending_recovered_batches_max: usize,
    pending_validation_reloads: usize,
    pending_validation_reloads_max: usize,
}

impl WindowServiceMetrics {
    const NAME: &str = "recv-window-insert-shreds";

    // Sample before and after each iteration, including startup and error paths.
    // Record the largest count in each reporting interval.
    // The current count shows how much work remains.
    fn record_pending_work(&mut self, tasks: usize, reloads: usize) {
        self.pending_recovery_tasks = tasks;
        self.pending_recovery_tasks_max = self.pending_recovery_tasks_max.max(tasks);
        self.pending_validation_reloads = reloads;
        self.pending_validation_reloads_max = self.pending_validation_reloads_max.max(reloads);
    }

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
            ("pending_recovery_tasks", self.pending_recovery_tasks, i64),
            (
                "pending_recovery_tasks_max",
                self.pending_recovery_tasks_max,
                i64
            ),
            (
                "pending_validation_reloads",
                self.pending_validation_reloads,
                i64
            ),
            (
                "pending_validation_reloads_max",
                self.pending_validation_reloads_max,
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

    // Count FEC results and reload IDs before and after each iteration.
    fn record_pending_work(&mut self, batches: usize, reloads: usize) {
        self.pending_recovered_batches = batches;
        self.pending_recovered_batches_max = self.pending_recovered_batches_max.max(batches);
        self.pending_validation_reloads = reloads;
        self.pending_validation_reloads_max = self.pending_validation_reloads_max.max(reloads);
    }

    fn record_task_batch(&mut self, num_tasks: usize, queue_depth: usize) {
        self.num_recovery_tasks += num_tasks;
        self.recovery_queue_depth_max = self.recovery_queue_depth_max.max(queue_depth);
    }

    fn report_metrics(&self) {
        datapoint_info!(
            Self::NAME,
            (
                "shred_recovery_elapsed_us",
                self.shred_recovery_elapsed_us,
                i64
            ),
            ("num_recovery_tasks", self.num_recovery_tasks, i64),
            ("num_recovered_batches", self.num_recovered_batches, i64),
            (
                "num_recovered_batches_dropped",
                self.num_recovered_batches_dropped,
                i64
            ),
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
            (
                "pending_recovered_batches",
                self.pending_recovered_batches,
                i64
            ),
            (
                "pending_recovered_batches_max",
                self.pending_recovered_batches_max,
                i64
            ),
            (
                "pending_validation_reloads",
                self.pending_validation_reloads,
                i64
            ),
            (
                "pending_validation_reloads_max",
                self.pending_validation_reloads_max,
                i64
            ),
        );
    }
}

fn run_check_duplicate(
    cluster_info: &ClusterInfo,
    blockstore: &Blockstore,
    shred_receiver: &Receiver<PossibleDuplicateShred>,
    duplicate_slots_sender: &DuplicateSlotSender,
    bank_forks: &RwLock<BankForks>,
) -> Result<()> {
    let migration_status = bank_forks.read().unwrap().migration_status();
    let check_duplicate = |shred: PossibleDuplicateShred| -> Result<()> {
        let shred_slot = shred.slot();
        let (shred1, shred2) = shred.into_proof();

        let should_mark_dead = migration_status
            .genesis_block()
            .is_some_and(|genesis| shred_slot > genesis.slot);
        if should_mark_dead {
            blockstore.set_dead_slot_if_duplicate_and_not_full(shred_slot)?;
        }

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
    recovery_sender: &Sender<Vec<ShredRecoveryTask>>,
    recovered_receiver: &Receiver<RecoveredShredBatch>,
    recovered_batches: &mut Vec<RecoveredShredBatch>,
    pending_tasks: &mut Vec<ShredRecoveryTask>,
    validation_reloads: &mut BTreeSet<ErasureSetId>,
    retransmit_sender: &RetransmitSender,
    stricter_shred_validation_from: Option<Slot>,
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
    let mut shreds = Vec::new();
    // Recovery must make progress even when no further network shreds arrive.
    select! {
        recv(verified_receiver) -> batch => {
            shreds = batch.map_err(|_| RecvTimeoutError::Disconnected)?;
        }
        recv(recovered_receiver) -> batch => {
            recovered_batches.push(batch.map_err(|_| RecvTimeoutError::Disconnected)?);
        }
        default(RECV_TIMEOUT) => {
            if pending_tasks.is_empty() && validation_reloads.is_empty() {
                return Err(RecvTimeoutError::Timeout.into());
            }
        },
    }
    shreds.extend(verified_receiver.try_iter().flatten());
    recovered_batches.extend(
        recovered_receiver
            .try_iter()
            .take(MAX_RECOVERED_BATCHES_PER_INSERT - recovered_batches.len()),
    );
    shred_receiver_elapsed.stop();
    ws_metrics.shred_receiver_elapsed_us += shred_receiver_elapsed.as_us();
    ws_metrics.run_insert_count += 1;
    let handle_shred = |(shred, repair, block_location): (shred::Payload, bool, BlockLocation)| {
        if repair {
            ws_metrics.num_repairs.fetch_add(1, Ordering::Relaxed);
        }
        let shred = Shred::new_from_serialized_shred(shred).ok()?;
        // Compute the Merkle root here while in the thread pool so the serial insertion
        // path can reuse it.
        let merkle_root = shred.merkle_root().ok();
        Some((Cow::Owned(shred), repair, block_location, merkle_root))
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
    let result = blockstore.insert_shreds_with_recovered(
        shreds,
        recovered_batches,
        retransmit_sender,
        stricter_shred_validation_from,
        pinnable_slice,
        write_batch,
        &handle_duplicate,
        metrics,
    );
    // The blockstore write can fail. A block switch can also invalidate a result.
    // In either case, replay still needs the SIMD-0504 validation result.
    // The worker has consumed the original task. Keep its ID to reload current inputs.
    for batch in recovered_batches.iter() {
        if batch.requires_validation() && (result.is_err() || !batch.has_validation_verdict()) {
            validation_reloads.insert(batch.erasure_set());
        }
    }
    recovered_batches.clear();
    let (completed_data_sets, mut recovery_tasks) = result?;
    if !validation_reloads.is_empty() {
        // Use tasks already in memory before reading their inputs from blockstore.
        for task in recovery_tasks.iter().chain(pending_tasks.iter()) {
            if task.requires_validation() {
                validation_reloads.remove(&task.erasure_set());
            }
        }
        // The remaining IDs have no usable task in memory.
        // If the channel is full, leave the IDs for a later iteration.
        // This avoids blockstore reads and task allocations until the channel has space.
        if !recovery_sender.is_full() {
            reload_validation_tasks(blockstore, validation_reloads, &mut recovery_tasks);
        }
    }

    if !pending_tasks.is_empty() {
        // Add new tasks after the pending tasks in the same buffer.
        // This order lets deduplication keep the newest task for each FEC set.
        // Swap the buffer into recovery_tasks to send it.
        // If the send fails because the channel is full, store the tasks in pending_tasks again.
        // If there are no new tasks, send the same batch without sorting or cloning.
        let has_new_tasks = !recovery_tasks.is_empty();
        pending_tasks.append(&mut recovery_tasks);
        std::mem::swap(pending_tasks, &mut recovery_tasks);
        if has_new_tasks {
            dedup_recovery_tasks(&mut recovery_tasks);
        }
    }

    ws_metrics.num_recovery_candidates += recovery_tasks.len();
    if !recovery_tasks.is_empty() {
        match recovery_sender.try_send(recovery_tasks) {
            Ok(()) => {}
            Err(TrySendError::Full(mut candidates)) => {
                let num_candidates = candidates.len();
                // Keep all tasks that require SIMD-0504 validation, including recovery tasks.
                // New arrivals can supply the missing data before these tasks run.
                // Repair then has no missing data to fetch, but replay still needs validation.
                // Discard tasks without required validation; repair can request missing data.
                candidates.retain(ShredRecoveryTask::requires_validation);
                ws_metrics.num_recovery_candidates_dropped += num_candidates - candidates.len();
                // Keep the tasks and their buffers. A full channel does not change their inputs.
                // We do not need to read them from blockstore again.
                *pending_tasks = candidates;
            }
            Err(TrySendError::Disconnected(_)) => return Err(Error::Send),
        }
    }

    if let Some(sender) = completed_data_sets_sender {
        sender.try_send(completed_data_sets)?;
    }

    Ok(())
}

fn dedup_recovery_tasks(recovery_tasks: &mut Vec<ShredRecoveryTask>) {
    // Reverse the tasks, then use a stable sort to put the newest task first per FEC set.
    // A block switch can replace shreds while their tasks are in the queue.
    recovery_tasks.reverse();
    recovery_tasks.sort_by_key(ShredRecoveryTask::erasure_set);
    recovery_tasks.dedup_by(|older, newer| {
        if older.erasure_set() != newer.erasure_set() {
            return false;
        }
        newer.merge_check_requirement(older);
        // A task without required validation cannot replace a required check for a different root.
        !older.requires_validation() || newer.requires_validation()
    });
}

/// Reload inputs after a restart or when a task's inputs or result cannot be used.
/// These IDs have no usable task in memory.
/// If a channel is full, keep the original task or result. No reload is needed.
fn reload_validation_tasks(
    blockstore: &Blockstore,
    pending: &mut BTreeSet<ErasureSetId>,
    tasks: &mut Vec<ShredRecoveryTask>,
) {
    pending.retain(|&id| match blockstore.prepare_shred_validation_retry(id) {
        Ok(task) => {
            tasks.extend(task);
            false
        }
        Err(err) => {
            warn!("Unable to reload validation task {id:?}: {err}");
            true
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn run_recovery<'db>(
    recovery_receiver: &Receiver<Vec<ShredRecoveryTask>>,
    recovered_sender: &Sender<RecoveredShredBatch>,
    validation_reloads: &mut BTreeSet<ErasureSetId>,
    pending_results: &mut BTreeMap<ErasureSetId, RecoveredShredBatch>,
    blockstore: &'db Blockstore,
    shred_recovery_context: &mut ShredRecoveryContext,
    pinnable_slice: &mut DBPinnableSlice<'db>,
    recovery_metrics: &mut WindowRecoveryMetrics,
) -> Result<()> {
    const RECV_TIMEOUT: Duration = Duration::from_millis(200);
    let mut recovery_tasks = match recovery_receiver.recv_timeout(RECV_TIMEOUT) {
        Ok(tasks) => tasks,
        Err(RecvTimeoutError::Timeout)
            if !validation_reloads.is_empty() || !pending_results.is_empty() =>
        {
            Vec::new()
        }
        Err(err) => return Err(err.into()),
    };
    // These validation results could not enter the full channel. Send them again
    // without repeating validation. Stop if the channel is still full.
    // This should be rare. Increase channel capacity if expected bursts fill it often.
    while let Some((id, batch)) = pending_results.pop_first() {
        match recovered_sender.try_send(batch) {
            Ok(()) => {}
            Err(TrySendError::Full(batch)) => {
                pending_results.insert(id, batch);
                break;
            }
            Err(TrySendError::Disconnected(_)) => return Err(Error::Send),
        }
    }
    let queue_depth = recovery_receiver.len();
    for tasks in recovery_receiver.try_iter().take(queue_depth) {
        recovery_tasks.extend(tasks);
    }
    let dedup = queue_depth > 0 || !validation_reloads.is_empty();
    if !validation_reloads.is_empty() {
        // An earlier attempt consumed its task but could not produce a validation result.
        // A read failure or a block switch can cause this.
        // Use new tasks from the queue first. Reload inputs only for the remaining IDs.
        for task in &recovery_tasks {
            if task.requires_validation() {
                validation_reloads.remove(&task.erasure_set());
            }
        }
        reload_validation_tasks(blockstore, validation_reloads, &mut recovery_tasks);
    }
    recovery_metrics.record_task_batch(recovery_tasks.len(), queue_depth);
    if dedup {
        dedup_recovery_tasks(&mut recovery_tasks);
    }
    for task in recovery_tasks {
        let erasure_set = task.erasure_set();
        if !task.must_check_completed_set() {
            match blockstore.is_erasure_set_complete(erasure_set, pinnable_slice) {
                Ok(true) => continue,
                Ok(false) => {}
                Err(err) => {
                    // Continue so later tasks in this batch can complete required SIMD-0504
                    // validation.
                    warn!("Unable to check recovery task {erasure_set:?}: {err}");
                    recovery_metrics.num_recovery_tasks_failed += 1;
                    continue;
                }
            }
        }
        let requires_validation = task.requires_validation();
        let mut batch = RecoveredShredBatch::new(erasure_set);
        let now = Instant::now();
        let recovered =
            blockstore.recover_shreds_from_task(task, shred_recovery_context, &mut batch);
        recovery_metrics.shred_recovery_elapsed_us += now.elapsed().as_micros() as u64;
        if !recovered {
            recovery_metrics.num_recovery_tasks_failed += 1;
            if requires_validation {
                // There is no validation result, so replay still needs this check.
                // Reload current inputs on a later iteration.
                // A failed FEC check returns a result so insertion can mark the slot dead.
                validation_reloads.insert(erasure_set);
            }
            continue;
        }
        recovery_metrics.num_recovered_batches += 1;

        match recovered_sender.try_send(batch) {
            Ok(()) => {}
            Err(TrySendError::Full(batch)) => {
                if batch.requires_validation() {
                    // All data may already be stored, so repair might not trigger another check.
                    // Keep this SIMD-0504 validation result until the channel accepts it.
                    // Insertion writes the result to allow replay or mark the slot dead.
                    pending_results.insert(batch.erasure_set(), batch);
                } else {
                    recovery_metrics.num_recovered_batches_dropped += 1;
                }
            }
            Err(TrySendError::Disconnected(_)) => return Err(Error::Send),
        }
    }
    Ok(())
}

pub struct WindowServiceChannels {
    pub verified_receiver: Receiver<Vec<(shred::Payload, /*is_repaired:*/ bool, BlockLocation)>>,
    pub retransmit_sender: RetransmitSender,
    pub completed_data_sets_sender: Option<CompletedDataSetsSender>,
    pub duplicate_slots_sender: DuplicateSlotSender,
    pub repair_service_channels: RepairServiceChannels,
    pub block_id_repair_channels: BlockIdRepairChannels,
}

impl WindowServiceChannels {
    pub fn new(
        verified_receiver: Receiver<Vec<(shred::Payload, /*is_repaired:*/ bool, BlockLocation)>>,
        retransmit_sender: RetransmitSender,
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
        let (recovery_sender, recovery_receiver) = bounded(RECOVERY_CHANNEL_CAPACITY);
        let (recovered_sender, recovered_receiver) = bounded(RECOVERED_CHANNEL_CAPACITY);
        let sharable_banks = bank_forks.read().unwrap().sharable_banks();
        let t_recovery = Self::start_window_recovery_thread(
            exit.clone(),
            blockstore.clone(),
            sharable_banks.clone(),
            shred_version,
            recovery_receiver,
            recovered_sender,
        );
        let t_insert = Self::start_window_insert_thread(
            exit,
            blockstore,
            sharable_banks,
            shred_version,
            verified_receiver,
            recovery_sender,
            recovered_receiver,
            retransmit_sender,
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

    #[allow(clippy::too_many_arguments)]
    fn start_window_insert_thread(
        exit: Arc<AtomicBool>,
        blockstore: Arc<Blockstore>,
        sharable_banks: SharableBanks,
        shred_version: u16,
        verified_receiver: Receiver<Vec<(shred::Payload, /*is_repaired:*/ bool, BlockLocation)>>,
        recovery_sender: Sender<Vec<ShredRecoveryTask>>,
        recovered_receiver: Receiver<RecoveredShredBatch>,
        retransmit_sender: RetransmitSender,
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
                let mut recovered_batches = Vec::new();
                // Keep SIMD-0504 validation tasks here if the channel is full.
                // Replay must wait for validation, even if all data shreds are stored.
                // If we discard these tasks, replay can remain blocked.
                // This buffer should normally be empty. Size the channel for expected bursts.
                // If work continues to accumulate, check the recovery worker's processing rate.
                // The channel capacity does not limit this buffer.
                let mut pending_tasks = Vec::new();
                let mut filter_context =
                    ShredFilterContext::new(sharable_banks.root(), shred_version);
                // Keep FEC set IDs that need new tasks from blockstore after restart,
                // a block switch, or a failed read or write. These IDs have no reusable task.
                // If the channel is full, keep the original tasks in pending_tasks instead.
                let mut validation_reloads = blockstore
                    .pending_shred_validation(filter_context.stricter_shred_validation_from())
                    .expect("load pending FEC validation");

                while !exit.load(Ordering::Relaxed) {
                    filter_context.maybe_update(sharable_banks.root());
                    ws_metrics.record_pending_work(pending_tasks.len(), validation_reloads.len());
                    if let Err(e) = run_insert(
                        &thread_pool,
                        &verified_receiver,
                        &recovery_sender,
                        &recovered_receiver,
                        &mut recovered_batches,
                        &mut pending_tasks,
                        &mut validation_reloads,
                        &retransmit_sender,
                        filter_context.stricter_shred_validation_from(),
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
                    ws_metrics.record_pending_work(pending_tasks.len(), validation_reloads.len());

                    if last_print.elapsed() > METRICS_REPORTING_INTERVAL {
                        metrics.report_metrics();
                        metrics = BlockstoreInsertionMetrics::default();
                        ws_metrics.report_metrics();
                        ws_metrics = WindowServiceMetrics::default();
                        last_print = Instant::now();
                    }
                }
            })
            .unwrap()
    }

    fn start_window_recovery_thread(
        exit: Arc<AtomicBool>,
        blockstore: Arc<Blockstore>,
        sharable_banks: SharableBanks,
        shred_version: u16,
        recovery_receiver: Receiver<Vec<ShredRecoveryTask>>,
        recovered_sender: Sender<RecoveredShredBatch>,
    ) -> JoinHandle<()> {
        Builder::new()
            .name("solWinRecover".to_string())
            .spawn(move || {
                let mut shred_recovery_context =
                    ShredRecoveryContext::new(sharable_banks.root(), shred_version);
                let mut pinnable_slice = blockstore.new_pinnable_slice();
                let mut recovery_metrics = WindowRecoveryMetrics::default();
                // Reload tasks if an earlier attempt could not use their inputs.
                // This set does not track tasks that are still in progress.
                let mut validation_reloads = BTreeSet::new();
                // Keep SIMD-0504 validation results here if the channel is full.
                // The insertion thread applies each result to blockstore: a successful check
                // sets the FEC validation bit; a failed check can mark the slot dead.
                // Keep the results to avoid repeat validation.
                // This buffer should normally be empty. Size the channel for expected bursts.
                // If work continues to accumulate, check the insertion thread's processing rate.
                // The channel capacity does not limit this buffer.
                let mut pending_results = BTreeMap::new();
                let mut last_print = Instant::now();
                const METRICS_REPORTING_INTERVAL: Duration = Duration::from_secs(2);

                while !exit.load(Ordering::Relaxed) {
                    shred_recovery_context.maybe_update(sharable_banks.root());
                    recovery_metrics
                        .record_pending_work(pending_results.len(), validation_reloads.len());
                    if let Err(e) = run_recovery(
                        &recovery_receiver,
                        &recovered_sender,
                        &mut validation_reloads,
                        &mut pending_results,
                        &blockstore,
                        &mut shred_recovery_context,
                        &mut pinnable_slice,
                        &mut recovery_metrics,
                    ) && Self::should_exit_on_error(e)
                    {
                        break;
                    }
                    recovery_metrics
                        .record_pending_work(pending_results.len(), validation_reloads.len());
                    if last_print.elapsed() > METRICS_REPORTING_INTERVAL {
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
        rand::Rng,
        solana_entry::entry::{Entry, create_ticks},
        solana_gossip::contact_info::ContactInfo,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_ledger::{
            blockstore::{Blockstore, make_many_slot_entries},
            get_tmp_ledger_path_auto_delete,
            shred::{
                ProcessShredsStats, Shredder,
                filter::{new_dummy_retransmit_sender, new_test_retransmit_sender},
            },
        },
        solana_net_utils::SocketAddrSpace,
        solana_pubkey::Pubkey,
        solana_runtime::{
            bank::Bank,
            genesis_utils::{
                bootstrap_validator_stake_lamports, create_genesis_config,
                create_genesis_config_with_tower_leader,
            },
        },
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

    // Repeated tasks must produce one FEC check, even if newer tasks only request recovery.
    // Incoming copies of recovered shreds must not produce duplicate proofs.
    // A recovery result must also wake insertion when no network shreds arrive.
    #[test_case::test_case(false, false; "recovery_completes_set_without_network_traffic")]
    #[test_case::test_case(true, false; "network_completes_set_before_recovery")]
    #[test_case::test_case(false, true; "network_and_recovery_share_insert")]
    fn test_run_recovery_skips_redundant_tasks(
        network_completes_set: bool,
        concurrent_arrival: bool,
    ) {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();
        let mut pinnable_slice = blockstore.new_pinnable_slice();
        let mut write_batch = blockstore.get_write_batch();
        let (mut data, coding) = Shredder::new(1, 0, 0, 0)
            .unwrap()
            .entries_to_merkle_shreds_for_tests(
                &Keypair::new(),
                &create_ticks(1, 0, Hash::default()),
                true,
                Hash::new_unique(),
                0,
                0,
                &mut ProcessShredsStats::default(),
            );
        let missing = data.pop().unwrap();
        let erasure_set = shred::ErasureSetId::new(missing.slot(), missing.fec_set_index());
        blockstore.insert_shreds(data, false).unwrap();
        assert!(
            !blockstore
                .is_erasure_set_complete(erasure_set, &mut pinnable_slice)
                .unwrap()
        );

        // Each new coding shred schedules recovery for the incomplete FEC set.
        // Keep a third task to simulate work that enters the queue during recovery.
        let mut task_batches = coding.into_iter().take(3).map(|shred| {
            let (_, tasks) = blockstore
                .insert_shreds_with_recovered(
                    [(Cow::Owned(shred), false, BlockLocation::Original, None)],
                    &mut [],
                    &new_dummy_retransmit_sender(),
                    None, // stricter_shred_validation_from
                    &mut pinnable_slice,
                    &mut write_batch,
                    &|_| {},
                    &mut BlockstoreInsertionMetrics::default(),
                )
                .unwrap();
            assert_eq!(tasks.len(), 1);
            tasks
        });
        let (sender, receiver) = bounded(4);
        sender.send(task_batches.next().unwrap()).unwrap();
        sender.send(task_batches.next().unwrap()).unwrap();
        let delayed_tasks = task_batches.next().unwrap();
        if network_completes_set {
            blockstore.insert_shreds([missing.clone()], false).unwrap();
        }

        let (retransmit_sender, retransmit_receiver) = new_test_retransmit_sender();
        let bank = Arc::new(Bank::new_for_tests(
            &create_genesis_config(2).genesis_config,
        ));
        let mut context = ShredRecoveryContext::new(bank, 0);
        let (recovered_sender, recovered_receiver) = bounded(4);
        let (verified_sender, verified_receiver) = bounded(4);
        let mut pending_tasks = Vec::new();
        let mut pending_insert = BTreeSet::new();
        let mut pending_recovery = BTreeSet::new();
        let mut pending_results = BTreeMap::new();
        let (recovery_sender, recovery_receiver) = bounded(4);
        let (completed_sender, completed_receiver) = bounded(4);
        let thread_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        let mut ws_metrics = WindowServiceMetrics::default();
        let mut metrics = BlockstoreInsertionMetrics::default();
        let mut recovery_metrics = WindowRecoveryMetrics::default();
        let mut recovered_batches = Vec::new();
        for tasks in [None, Some(delayed_tasks)] {
            if let Some(tasks) = tasks {
                sender.send(tasks).unwrap();
            }
            run_recovery(
                &receiver,
                &recovered_sender,
                &mut pending_recovery,
                &mut pending_results,
                &blockstore,
                &mut context,
                &mut pinnable_slice,
                &mut recovery_metrics,
            )
            .unwrap();
            if !recovered_receiver.is_empty() {
                // Reconstruction must not insert or retransmit shreds.
                assert_eq!(
                    blockstore
                        .is_erasure_set_complete(erasure_set, &mut pinnable_slice)
                        .unwrap(),
                    network_completes_set,
                );
                assert!(retransmit_receiver.is_empty());
                if concurrent_arrival {
                    verified_sender
                        .send(vec![(
                            missing.payload().clone(),
                            false,
                            BlockLocation::Original,
                        )])
                        .unwrap();
                }
                run_insert(
                    &thread_pool,
                    &verified_receiver,
                    &recovery_sender,
                    &recovered_receiver,
                    &mut recovered_batches,
                    &mut pending_tasks,
                    &mut pending_insert,
                    &retransmit_sender,
                    None,
                    &blockstore,
                    &mut pinnable_slice,
                    &mut write_batch,
                    |_| panic!("matching recovered shreds are not duplicate proofs"),
                    &mut metrics,
                    &mut ws_metrics,
                    Some(&completed_sender),
                )
                .unwrap();
                assert!(recovered_receiver.is_empty());
                assert!(verified_receiver.is_empty());
                assert!(recovery_receiver.is_empty());
                assert_eq!(
                    completed_receiver.try_recv().unwrap().len(),
                    usize::from(!network_completes_set)
                );
            }
            assert!(receiver.is_empty());
            assert!(
                blockstore
                    .is_erasure_set_complete(erasure_set, &mut pinnable_slice)
                    .unwrap()
            );
        }
        assert_eq!(recovery_metrics.num_recovery_tasks, 3);
        // Before feature activation, the first recoverable task must still check the FEC set.
        // This check must run even if network traffic has supplied all data shreds.
        assert_eq!(recovery_metrics.num_recovered_batches, 1);
        assert_eq!(metrics.num_fec_set_checks_passed, 1);
        assert_eq!(recovery_metrics.num_recovery_tasks_failed, 0);
        assert_eq!(
            metrics.num_recovered_exists,
            usize::from(concurrent_arrival || network_completes_set)
        );
        assert_eq!(
            blockstore
                .get_data_shred(missing.slot(), u64::from(missing.index()))
                .unwrap()
                .unwrap(),
            missing.payload().as_ref()
        );
        // Dropping the pool waits for all retransmit jobs to reach the receiver.
        drop(retransmit_sender);
        assert_eq!(retransmit_receiver.len(), 2);
    }

    // Required validation must survive a full task channel or result channel without new shreds or repair.
    // Retries must reuse task inputs and completed results, without blockstore reloads or repeated validation.
    // A newer task for a replacement block must replace the unsent task for the old block.
    #[test_case::test_case(false, false; "task_queue_full")]
    #[test_case::test_case(true, false; "result_queue_full")]
    #[test_case::test_case(false, true; "new_snapshot_replaces_unsent_task")]
    fn test_validation_retries_full_channels_without_repair(
        full_results: bool,
        replace_pending: bool,
    ) {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();
        let (verified_sender, verified_receiver) = bounded(4);
        let mut pending_tasks = Vec::new();
        let mut pending_insert = BTreeSet::new();
        let mut pending_recovery = BTreeSet::new();
        let mut pending_results = BTreeMap::new();
        let (recovery_sender, recovery_receiver) = bounded(1);
        let (recovered_sender, recovered_receiver) = bounded(if full_results { 1 } else { 2 });
        let retransmit_sender = new_dummy_retransmit_sender();
        let thread_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        let mut metrics = BlockstoreInsertionMetrics::default();
        let mut ws_metrics = WindowServiceMetrics::default();
        let mut recovery_metrics = WindowRecoveryMetrics::default();
        let mut context = ShredRecoveryContext::new(
            Arc::new(Bank::new_for_tests(
                &create_genesis_config(2).genesis_config,
            )),
            0,
        );
        let mut scratch = Vec::new();
        {
            let mut insert = |incoming: Vec<(shred::Payload, bool, BlockLocation)>| {
                if !incoming.is_empty() {
                    verified_sender.send(incoming).unwrap();
                }
                run_insert(
                    &thread_pool,
                    &verified_receiver,
                    &recovery_sender,
                    &recovered_receiver,
                    &mut scratch,
                    &mut pending_tasks,
                    &mut pending_insert,
                    &retransmit_sender,
                    Some(0),
                    &blockstore,
                    &mut blockstore.new_pinnable_slice(),
                    &mut blockstore.get_write_batch(),
                    |_| panic!("unexpected duplicate proof"),
                    &mut metrics,
                    &mut ws_metrics,
                    None,
                )
                .unwrap();
                assert!(pending_insert.is_empty()); // No input reloads for channel backpressure.
                pending_tasks.len()
            };
            let mut recover = || {
                run_recovery(
                    &recovery_receiver,
                    &recovered_sender,
                    &mut pending_recovery,
                    &mut pending_results,
                    &blockstore,
                    &mut context,
                    &mut blockstore.new_pinnable_slice(),
                    &mut recovery_metrics,
                )
                .unwrap();
                (pending_recovery.len(), pending_results.len())
            };
            let make_payloads = |slot| {
                local_entries_to_shred(
                    &create_ticks(1, 0, Hash::default()),
                    slot,
                    0,
                    &Keypair::new(),
                )
                .into_iter()
                .map(|shred| (shred.into_payload(), false, BlockLocation::Original))
                .collect::<Vec<_>>()
            };
            let first = make_payloads(100);
            let second = make_payloads(101);
            if full_results {
                // Both tasks are accepted, but only the first result fits.
                assert_eq!(insert(first.into_iter().chain(second).collect()), 0);
            } else {
                // The second task is refused; the first remains in the channel.
                assert_eq!(insert(first), 0);
                assert_eq!(insert(second), 1);
                if replace_pending {
                    // Keep the first task in the channel.
                    // Replace only the unsent second task with inputs from the new block.
                    blockstore.clear_unconfirmed_slot(101);
                    assert_eq!(insert(make_payloads(101)), 1);
                }
                // If the channel is still full after a timeout, keep the whole second task.
                // Do not submit the first task again; the channel already accepted it.
                assert_eq!(insert(vec![]), 1);
            }
            assert_eq!(recover(), (0, usize::from(full_results)));
            if full_results {
                // If the result channel is still full after another timeout, keep the result.
                // Do not run validation again.
                assert_eq!(recover(), (0, 1));
            }
            // Insertion writes the first validation result to blockstore.
            // If the task channel was full, insertion can now send the second task.
            assert_eq!(insert(vec![]), 0);
            assert!(
                blockstore
                    .meta(100)
                    .unwrap()
                    .unwrap()
                    .is_full_and_validated()
            );
            assert!(
                !blockstore
                    .meta(101)
                    .unwrap()
                    .unwrap()
                    .is_full_and_validated()
            );
            assert!(verified_receiver.is_empty());
            if full_results {
                // No new input arrives. After the receive timeout, send the stored result again.
                // Do not repeat reconstruction or validation.
                assert!(recovery_receiver.is_empty());
            }
            assert_eq!(recover(), (0, 0));
            assert_eq!(insert(vec![]), 0);
        }
        assert!(pending_insert.is_empty());
        assert!(pending_tasks.is_empty());
        assert!(pending_recovery.is_empty());
        assert!(pending_results.is_empty());
        assert!(
            blockstore
                .meta(101)
                .unwrap()
                .unwrap()
                .is_full_and_validated()
        );
        assert!(!blockstore.is_dead(100));
        assert_eq!(ws_metrics.num_recovery_candidates_dropped, 0);
        assert_eq!(recovery_metrics.num_recovered_batches_dropped, 0);
        // A retry must not repeat recovery or validation.
        assert_eq!(recovery_metrics.num_recovered_batches, 2);
        assert_eq!(recovery_metrics.num_recovery_tasks, 2);
        assert_eq!(metrics.num_fec_set_checks_passed, 2);
        assert_eq!(metrics.num_fec_set_checks_failed, 0);
    }

    // A block switch can invalidate inputs in the worker or a result returned to insertion.
    // Both paths must reload the replacement block and finish validation without new network traffic.
    #[test_case::test_case(true; "stale_input_snapshot")]
    #[test_case::test_case(false; "stale_returned_verdict")]
    fn test_validation_requeues_after_block_switch(before_recovery: bool) {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();
        let slot = 100;
        let make_shreds = || {
            local_entries_to_shred(
                &create_ticks(1, 0, Hash::default()),
                slot,
                0,
                &Keypair::new(),
            )
        };
        let insert_direct = |shreds: Vec<Shred>| {
            blockstore
                .insert_shreds_with_recovered(
                    shreds
                        .into_iter()
                        .map(|shred| (Cow::Owned(shred), false, BlockLocation::Original, None)),
                    &mut [],
                    &new_dummy_retransmit_sender(),
                    Some(0),
                    &mut blockstore.new_pinnable_slice(),
                    &mut blockstore.get_write_batch(),
                    &|_| panic!("unexpected duplicate proof"),
                    &mut BlockstoreInsertionMetrics::default(),
                )
                .unwrap()
                .1
        };
        let mut old_shreds = make_shreds();
        let last = old_shreds.pop().unwrap();
        // Make the task load old shreds from blockstore.
        // A block switch can then invalidate the inputs before recovery produces a result.
        blockstore.insert_shreds(old_shreds, false).unwrap();
        let tasks = insert_direct(vec![last]);
        assert_eq!(tasks.len(), 1);
        let (task_sender, task_receiver) = bounded(2);
        task_sender.try_send(tasks).unwrap();
        let (result_sender, result_receiver) = bounded(2);
        let mut pending_tasks = Vec::new();
        let mut pending_insert = BTreeSet::new();
        let mut pending_recovery = BTreeSet::new();
        let mut pending_results = BTreeMap::new();
        let (_verified_sender, verified_receiver) = bounded(1);
        let retransmit_sender = new_dummy_retransmit_sender();
        let thread_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        let mut context = ShredRecoveryContext::new(
            Arc::new(Bank::new_for_tests(
                &create_genesis_config(2).genesis_config,
            )),
            0,
        );
        let mut recovery_metrics = WindowRecoveryMetrics::default();
        let mut metrics = BlockstoreInsertionMetrics::default();
        let mut ws_metrics = WindowServiceMetrics::default();
        let mut scratch = Vec::new();
        {
            let mut recover = || {
                run_recovery(
                    &task_receiver,
                    &result_sender,
                    &mut pending_recovery,
                    &mut pending_results,
                    &blockstore,
                    &mut context,
                    &mut blockstore.new_pinnable_slice(),
                    &mut recovery_metrics,
                )
                .unwrap();
                pending_recovery.len()
            };
            let mut insert = || {
                run_insert(
                    &thread_pool,
                    &verified_receiver,
                    &task_sender,
                    &result_receiver,
                    &mut scratch,
                    &mut pending_tasks,
                    &mut pending_insert,
                    &retransmit_sender,
                    Some(0),
                    &blockstore,
                    &mut blockstore.new_pinnable_slice(),
                    &mut blockstore.get_write_batch(),
                    |_| panic!("unexpected duplicate proof"),
                    &mut metrics,
                    &mut ws_metrics,
                    None,
                )
                .unwrap()
            };
            if !before_recovery {
                recover();
                assert_eq!(result_receiver.len(), 1);
            }
            blockstore.clear_unconfirmed_slot(slot);
            // The old inputs or result must trigger a reload of the current FEC set.
            assert_eq!(insert_direct(make_shreds()).len(), 1);
            if before_recovery {
                assert_eq!(recover(), 1);
                assert!(result_receiver.is_empty());
                assert!(task_receiver.is_empty());
                // After the receive timeout, the worker reloads inputs for the failed task.
            } else {
                insert();
                // Insertion reloads inputs after rejecting the old result, then sends the new task.
                assert_eq!(task_receiver.len(), 1);
            }
            assert!(
                !blockstore
                    .meta(slot)
                    .unwrap()
                    .unwrap()
                    .is_full_and_validated()
            );
            assert!(verified_receiver.is_empty());
            recover();
            insert();
        }
        assert!(
            blockstore
                .meta(slot)
                .unwrap()
                .unwrap()
                .is_full_and_validated()
        );
        assert!(!blockstore.is_dead(slot));
        assert_eq!(
            recovery_metrics.num_recovery_tasks_failed,
            usize::from(before_recovery)
        );
        assert_eq!(metrics.num_fec_set_checks_passed, 1);
        assert!(pending_insert.is_empty());
        assert!(pending_tasks.is_empty());
        assert!(pending_recovery.is_empty());
        assert!(pending_results.is_empty());
        assert!(result_receiver.is_empty());
        assert!(task_receiver.is_empty());
    }

    // Restart must find validation work lost from memory and submit it even without network traffic.
    // Skip work that becomes rooted or already validated before a retry.
    #[test]
    fn test_pending_validation_after_restart() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let mut blockstore = Blockstore::open(ledger_path.path()).unwrap();
        for slot in [100, 101] {
            let shreds = local_entries_to_shred(
                &create_ticks(1, 0, Hash::default()),
                slot,
                0,
                &Keypair::new(),
            );
            let (_, tasks) = blockstore
                .insert_shreds_with_recovered(
                    shreds
                        .into_iter()
                        .map(|shred| (Cow::Owned(shred), false, BlockLocation::Original, None)),
                    &mut [],
                    &new_dummy_retransmit_sender(),
                    Some(0),
                    &mut blockstore.new_pinnable_slice(),
                    &mut blockstore.get_write_batch(),
                    &|_| panic!("unexpected duplicate proof"),
                    &mut BlockstoreInsertionMetrics::default(),
                )
                .unwrap();
            assert_eq!(tasks.len(), 1);
            // Drop the tasks to simulate a process that stopped before recovery.
        }
        drop(blockstore);
        blockstore = Blockstore::open(ledger_path.path()).unwrap();
        let mut pending_tasks = Vec::new();
        let mut pending_insert = blockstore.pending_shred_validation(Some(0)).unwrap();
        assert_eq!(pending_insert.len(), 2);
        blockstore.set_roots(std::iter::once(&100)).unwrap();
        let (task_sender, task_receiver) = bounded(1);
        let (result_sender, result_receiver) = bounded(2);
        let (_verified_sender, verified_receiver) = bounded(1);
        let retransmit_sender = new_dummy_retransmit_sender();
        let thread_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        let mut context = ShredRecoveryContext::new(
            Arc::new(Bank::new_for_tests(
                &create_genesis_config(2).genesis_config,
            )),
            0,
        );
        let mut pending_recovery = BTreeSet::new();
        let mut pending_results = BTreeMap::new();
        let mut recovery_metrics = WindowRecoveryMetrics::default();
        let mut metrics = BlockstoreInsertionMetrics::default();
        let mut ws_metrics = WindowServiceMetrics::default();
        let mut scratch = Vec::new();
        let mut insert = |pending: &mut BTreeSet<ErasureSetId>| {
            run_insert(
                &thread_pool,
                &verified_receiver,
                &task_sender,
                &result_receiver,
                &mut scratch,
                &mut pending_tasks,
                pending,
                &retransmit_sender,
                Some(0),
                &blockstore,
                &mut blockstore.new_pinnable_slice(),
                &mut blockstore.get_write_batch(),
                |_| panic!("unexpected duplicate proof"),
                &mut metrics,
                &mut ws_metrics,
                None,
            )
            .unwrap()
        };
        // No network traffic or results arrive. The receive timeout triggers startup work.
        insert(&mut pending_insert);
        assert!(pending_insert.is_empty()); // Accepted jobs are no longer retained.
        assert_eq!(task_receiver.len(), 1);
        run_recovery(
            &task_receiver,
            &result_sender,
            &mut pending_recovery,
            &mut pending_results,
            &blockstore,
            &mut context,
            &mut blockstore.new_pinnable_slice(),
            &mut recovery_metrics,
        )
        .unwrap();
        assert_eq!(recovery_metrics.num_recovery_tasks, 1);
        insert(&mut pending_insert);
        assert!(
            blockstore
                .meta(101)
                .unwrap()
                .unwrap()
                .is_full_and_validated()
        );
        // Discard work that is no longer needed.
        pending_insert.insert(ErasureSetId::new(101, 0));
        insert(&mut pending_insert);
        assert!(pending_insert.is_empty());
        assert!(task_receiver.is_empty());
        assert!(pending_recovery.is_empty());
        assert!(pending_results.is_empty());
        assert!(result_receiver.is_empty());
    }

    #[test]
    fn test_run_check_duplicate() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let genesis_config = create_genesis_config_with_tower_leader(
            10_000,
            &Pubkey::new_unique(),
            bootstrap_validator_stake_lamports(),
        )
        .genesis_config;
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
            .send(PossibleDuplicateShred::MerkleRootConflict(
                duplicate_shred,
                original_shred.into_payload(),
            ))
            .unwrap();
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

        // Make sure a duplicate signal was sent
        assert_eq!(
            duplicate_slot_receiver.try_recv().unwrap(),
            duplicate_shred_slot
        );
    }

    /// A conflicting shred at an occupied index stores the proof and marks the non-full
    /// slot dead at insert time.
    #[test]
    fn test_run_check_duplicate_marks_existing_index_conflict_dead() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let genesis_config = create_genesis_config(10_000).genesis_config;
        let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis_config));
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();
        let (sender, receiver) = bounded(1024);
        let (duplicate_slot_sender, _duplicate_slot_receiver) = bounded(1024);
        let (shreds, _) = make_many_slot_entries(5, 1, 10);
        let original_shred = shreds[0].clone();
        let duplicate_shred = {
            let (mut shreds, _) = make_many_slot_entries(5, 1, 10);
            shreds.swap_remove(0)
        };
        let slot = original_shred.slot();
        blockstore
            .insert_shreds(vec![original_shred.clone()], false)
            .unwrap();

        blockstore
            .insert_shreds_with_recovered(
                [(
                    Cow::Borrowed(&duplicate_shred),
                    false,
                    BlockLocation::Original,
                    None,
                )],
                &mut [],
                &new_dummy_retransmit_sender(),
                None,
                &mut blockstore.new_pinnable_slice(),
                &mut blockstore.get_write_batch(),
                &|shred| sender.send(shred).unwrap(),
                &mut BlockstoreInsertionMetrics::default(),
            )
            .unwrap();
        assert!(blockstore.has_duplicate_shreds_in_slot(slot));
        assert!(
            blockstore.is_dead(slot),
            "duplicate slot that is not full must be marked dead at insert time"
        );

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
        assert!(
            blockstore.is_dead(slot),
            "duplicate slot that is not full must be marked dead"
        );
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
        let genesis_config = create_genesis_config_with_tower_leader(
            10_000,
            &Pubkey::new_unique(),
            bootstrap_validator_stake_lamports(),
        )
        .genesis_config;
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
                        None,
                    )
                });
            blockstore
                .insert_shreds_with_recovered(
                    shreds,
                    &mut [],
                    &new_dummy_retransmit_sender(),
                    None, // stricter_shred_validation_from
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
