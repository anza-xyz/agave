//! The block creation loop
//! When our leader window is reached, attempts to create our leader blocks
//! within the block timeouts. Responsible for inserting empty banks for
//! banking stage to fill, and clearing banks once the timeout has been reached.
//!
//! Once alpenglow is active, this is the only thread that will touch the [`PohRecorder`].
use {
    crate::{
        banking_trace::BankingTracer,
        replay_stage::{Finalizer, ReplayStage},
    },
    agave_votor::{common::block_timeout, event::LeaderWindowInfo},
    crossbeam_channel::Receiver,
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_hash::Hash,
    solana_ledger::{
        blockstore::Blockstore,
        leader_schedule_cache::LeaderScheduleCache,
        leader_schedule_utils::{last_of_consecutive_leader_slots, leader_slot_index},
    },
    solana_measure::measure::Measure,
    solana_poh::{
        poh_recorder::{PohRecorder, PohRecorderError, GRACE_TICKS_FACTOR, MAX_GRACE_SLOTS},
        record_channels::RecordReceiver,
    },
    solana_pubkey::Pubkey,
    solana_rpc::{rpc_subscriptions::RpcSubscriptions, slot_status_notifier::SlotStatusNotifier},
    solana_runtime::{
        bank::{Bank, NewBankOptions},
        bank_forks::BankForks,
    },
    stats::{LoopMetrics, SlotMetrics},
    std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Condvar, Mutex, RwLock,
        },
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
    thiserror::Error,
};

mod stats;

pub struct BlockCreationLoop {
    thread: JoinHandle<()>,
}

impl BlockCreationLoop {
    pub fn new(config: BlockCreationLoopConfig) -> Self {
        let thread = Builder::new()
            .name("solBlkCreatLoop".to_string())
            .spawn(move || {
                info!("BlockCreationLoop has started");
                start_loop(config);
                info!("BlockCreationLoop has stopped");
            })
            .unwrap();

        Self { thread }
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread.join()
    }
}

pub struct BlockCreationLoopConfig {
    pub exit: Arc<AtomicBool>,

    // Shared state
    pub bank_forks: Arc<RwLock<BankForks>>,
    pub blockstore: Arc<Blockstore>,
    pub cluster_info: Arc<ClusterInfo>,
    pub poh_recorder: Arc<RwLock<PohRecorder>>,
    pub leader_schedule_cache: Arc<LeaderScheduleCache>,
    pub rpc_subscriptions: Option<Arc<RpcSubscriptions>>,

    // Notifiers
    pub banking_tracer: Arc<BankingTracer>,
    pub slot_status_notifier: Option<SlotStatusNotifier>,

    // Receivers / notifications from banking stage / replay / votor
    pub leader_window_info_receiver: Receiver<LeaderWindowInfo>,
    pub highest_parent_ready: Arc<RwLock<(Slot, (Slot, Hash))>>,
    pub replay_highest_frozen: Arc<ReplayHighestFrozen>,

    // Channel to receive RecordReceiver from PohService
    pub record_receiver_receiver: Receiver<RecordReceiver>,
}

struct LeaderContext {
    exit: Arc<AtomicBool>,
    my_pubkey: Pubkey,
    leader_window_info_receiver: Receiver<LeaderWindowInfo>,
    highest_parent_ready: Arc<RwLock<(Slot, (Slot, Hash))>>,

    blockstore: Arc<Blockstore>,
    record_receiver: RecordReceiver,
    poh_recorder: Arc<RwLock<PohRecorder>>,
    leader_schedule_cache: Arc<LeaderScheduleCache>,
    bank_forks: Arc<RwLock<BankForks>>,
    rpc_subscriptions: Option<Arc<RpcSubscriptions>>,
    slot_status_notifier: Option<SlotStatusNotifier>,
    banking_tracer: Arc<BankingTracer>,
    replay_highest_frozen: Arc<ReplayHighestFrozen>,

    // Metrics
    metrics: LoopMetrics,
    slot_metrics: SlotMetrics,
}

#[derive(Default)]
pub struct ReplayHighestFrozen {
    pub highest_frozen_slot: Mutex<Slot>,
    pub freeze_notification: Condvar,
}

#[derive(Debug, Error)]
enum StartLeaderError {
    /// Replay has not yet frozen the parent slot
    #[error("Replay is behind for parent slot {0} for leader slot {1}")]
    ReplayIsBehind(/* parent slot */ Slot, /* leader slot */ Slot),

    /// Bank forks already contains bank
    #[error("Already contain bank for leader slot {0}")]
    AlreadyHaveBank(/* leader slot */ Slot),

    /// Cluster has certified blocks after our leader window
    #[error("Cluster has certified blocks before {0} which is after our leader slot {1}")]
    ClusterCertifiedBlocksAfterWindow(
        /* parent ready slot */ Slot,
        /* leader slot */ Slot,
    ),
}

/// The block creation loop.
///
/// The `votor::consensus_pool_service` tracks when it is our leader window, and
/// communicates the skip timer and parent slot for our window. This loop takes the responsibility
/// of creating our `NUM_CONSECUTIVE_LEADER_SLOTS` blocks and finishing them within the required timeout.
fn start_loop(config: BlockCreationLoopConfig) {
    let BlockCreationLoopConfig {
        exit,
        bank_forks,
        blockstore,
        cluster_info,
        poh_recorder,
        leader_schedule_cache,
        rpc_subscriptions,
        banking_tracer,
        slot_status_notifier,
        leader_window_info_receiver,
        highest_parent_ready,
        replay_highest_frozen,
        record_receiver_receiver,
    } = config;

    // Similar to Votor, if this loop dies kill the validator
    let _exit = Finalizer::new(exit.clone());

    // get latest identity pubkey during startup
    let mut my_pubkey = cluster_info.id();

    // Wait for PohService to be shutdown
    let record_receiver = match record_receiver_receiver.recv() {
        Ok(receiver) => receiver,
        Err(e) => {
            error!("{my_pubkey}: Failed to receive RecordReceiver from PohService. Exiting: {e:?}",);
            return;
        }
    };

    info!("{my_pubkey}: PohService has shutdown, BlockCreationLoop is enabled");

    let mut ctx = LeaderContext {
        exit,
        my_pubkey,
        leader_window_info_receiver,
        highest_parent_ready,
        blockstore,
        record_receiver,
        poh_recorder: poh_recorder.clone(),
        leader_schedule_cache,
        bank_forks,
        rpc_subscriptions,
        slot_status_notifier,
        banking_tracer,
        replay_highest_frozen,
        metrics: LoopMetrics::default(),
        slot_metrics: SlotMetrics::default(),
    };

    // Setup poh
    // Important this is called *before* any new alpenglow
    // leaders call `set_bank()`, otherwise, the old PoH
    // tick producer will still tick in that alpenglow bank
    // Since we received the `record_receiver` from PohService, we know that PohService has shutdown
    // AND that replay no longer touches poh recorder. At this point BlockCreationLoop is the sole
    // modifier of poh_recorder
    {
        let mut w_poh_recorder = ctx.poh_recorder.write().unwrap();
        w_poh_recorder.enable_alpenglow();
    }
    reset_poh_recorder(&ctx.bank_forks.read().unwrap().working_bank(), &ctx);

    while !ctx.exit.load(Ordering::Relaxed) {
        // Check if set-identity was called at each leader window start
        if my_pubkey != cluster_info.id() {
            let my_old_pubkey = my_pubkey;
            my_pubkey = cluster_info.id();
            ctx.my_pubkey = my_pubkey;

            warn!(
                "Identity changed from {my_old_pubkey} to {my_pubkey} during block creation loop"
            );
        }

        // Wait for the voting loop to notify us, draining all pending messages and keeping the highest slot
        let LeaderWindowInfo {
            start_slot,
            end_slot,
            parent_block: (parent_slot, _),
            skip_timer,
        } = {
            // Drain all pending messages and keep the latest one
            let Some(info) = ctx
                .leader_window_info_receiver
                // Timeout so we can check the exit flag
                .recv_timeout(Duration::from_secs(1))
                .ok()
                .and_then(|window| {
                    ctx.leader_window_info_receiver
                        .try_iter()
                        .last()
                        .or(Some(window))
                })
            else {
                continue;
            };

            info
        };

        trace!("Received window notification for {start_slot} to {end_slot} parent: {parent_slot}");
        if let Err(e) = produce_window(start_slot, end_slot, parent_slot, skip_timer, &mut ctx) {
            // Give up on this leader window
            error!(
                "{my_pubkey}: Unable to produce window {start_slot}-{end_slot}, skipping window: \
                 {e:?}"
            );
        }

        ctx.metrics.loop_count += 1;
        ctx.metrics.report(Duration::from_secs(1));
    }
}

/// Resets poh recorder
fn reset_poh_recorder(bank: &Arc<Bank>, ctx: &LeaderContext) {
    trace!("{}: resetting poh to {}", ctx.my_pubkey, bank.slot());
    assert!(ctx.record_receiver.is_shutdown() && ctx.record_receiver.is_safe_to_restart());
    let next_leader_slot = ctx.leader_schedule_cache.next_leader_slot(
        &ctx.my_pubkey,
        bank.slot(),
        bank,
        Some(ctx.blockstore.as_ref()),
        GRACE_TICKS_FACTOR * MAX_GRACE_SLOTS,
    );

    ctx.poh_recorder
        .write()
        .unwrap()
        .reset(bank.clone(), next_leader_slot);
}

/// Produces the leader window from `start_slot` -> `end_slot` using parent
/// `parent_slot` while abiding to the `skip_timer`
fn produce_window(
    start_slot: Slot,
    end_slot: Slot,
    mut parent_slot: Slot,
    skip_timer: Instant,
    ctx: &mut LeaderContext,
) -> Result<(), StartLeaderError> {
    let my_pubkey = ctx.my_pubkey;
    let mut window_production_start = Measure::start("window_production");
    let mut slot = start_slot;

    while !ctx.exit.load(Ordering::Relaxed) && slot <= end_slot {
        // Insert the bank. In case `replay_stage` is slow and `parent_slot` is not
        // yet frozen, we wait up until the timeout.
        start_leader_wait_for_parent_replay(slot, parent_slot, skip_timer, ctx)?;

        let leader_index = leader_slot_index(slot);
        let timeout = block_timeout(leader_index);
        trace!(
            "{my_pubkey}: waiting for leader bank {slot} to finish, remaining time: {}",
            timeout.saturating_sub(skip_timer.elapsed()).as_millis(),
        );

        let mut bank_completion_measure = Measure::start("bank_completion");
        if let Err(e) = record_and_complete_block(
            ctx.poh_recorder.as_ref(),
            &mut ctx.record_receiver,
            skip_timer,
            timeout,
        ) {
            panic!("PohRecorder record failed: {e:?}");
        }
        assert!(!ctx.poh_recorder.read().unwrap().has_bank());
        bank_completion_measure.stop();
        ctx.slot_metrics.report();

        ctx.metrics.bank_timeout_completion_count += 1;
        let _ = ctx
            .metrics
            .bank_timeout_completion_elapsed_hist
            .increment(bank_completion_measure.as_us())
            .inspect_err(|e| {
                error!(
                    "{}: unable to increment bank completion histogram {e:?}",
                    ctx.my_pubkey
                );
            });

        // Produce our next slot
        parent_slot = slot;
        slot += 1;
    }
    trace!("{my_pubkey}: finished leader window {start_slot}-{end_slot}");

    window_production_start.stop();
    ctx.metrics.window_production_elapsed += window_production_start.as_us();
    Ok(())
}

/// Records incoming transactions until we reach the block timeout.
/// Afterwards:
/// - Shutdown the record receiver
/// - Clear any inflight records
/// - Insert the alpentick
/// - Clear the working bank
fn record_and_complete_block(
    poh_recorder: &RwLock<PohRecorder>,
    record_receiver: &mut RecordReceiver,
    block_timer: Instant,
    block_timeout: Duration,
) -> Result<(), PohRecorderError> {
    // loop until we hit the block timeout
    while !block_timeout
        .saturating_sub(block_timer.elapsed())
        .is_zero()
    {
        let Ok(record) = record_receiver.try_recv() else {
            continue;
        };
        poh_recorder.write().unwrap().record(
            record.bank_id,
            record.mixins,
            record.transaction_batches,
        )?;
    }

    // Shutdown and clear any inflight records
    record_receiver.shutdown();
    while !record_receiver.is_safe_to_restart() {
        let Ok(record) = record_receiver.recv_timeout(Duration::ZERO) else {
            continue;
        };
        poh_recorder.write().unwrap().record(
            record.bank_id,
            record.mixins,
            record.transaction_batches,
        )?;
    }

    // Alpentick and clear bank
    let mut w_poh_recorder = poh_recorder.write().unwrap();
    let bank = w_poh_recorder
        .bank()
        .expect("Bank cannot have been cleared as BlockCreationLoop is the only modifier");

    trace!(
        "{}: bank {} has reached block timeout, ticking",
        bank.collector_id(),
        bank.slot()
    );

    let max_tick_height = bank.max_tick_height();
    // Set the tick height for the bank to max_tick_height - 1, so that PohRecorder::flush_cache()
    // will properly increment the tick_height to max_tick_height.
    bank.set_tick_height(max_tick_height - 1);
    // Write the single tick for this slot
    drop(bank);
    w_poh_recorder.tick_alpenglow(max_tick_height);

    Ok(())
}

/// Similar to `maybe_start_leader`, however if replay of the parent block is lagging we retry
/// until either replay finishes or we hit the block timeout.
fn start_leader_wait_for_parent_replay(
    slot: Slot,
    parent_slot: Slot,
    skip_timer: Instant,
    ctx: &mut LeaderContext,
) -> Result<(), StartLeaderError> {
    trace!(
        "{}: Attempting to start leader slot {slot} parent {parent_slot}",
        ctx.my_pubkey
    );
    let my_pubkey = ctx.my_pubkey;
    let timeout = block_timeout(leader_slot_index(slot));
    let end_slot = last_of_consecutive_leader_slots(slot);

    let mut slot_delay_start = Measure::start("slot_delay");
    while !timeout.saturating_sub(skip_timer.elapsed()).is_zero() {
        ctx.slot_metrics.attempt_start_leader_count += 1;

        // Check if the entire window is skipped.
        let highest_parent_ready_slot = ctx.highest_parent_ready.read().unwrap().0;
        if highest_parent_ready_slot > end_slot {
            trace!(
                "{my_pubkey}: Skipping production of {slot} because highest parent ready slot is \
                 {highest_parent_ready_slot} > end slot {end_slot}"
            );
            ctx.metrics.skipped_window_behind_parent_ready_count += 1;
            return Err(StartLeaderError::ClusterCertifiedBlocksAfterWindow(
                highest_parent_ready_slot,
                slot,
            ));
        }

        match maybe_start_leader(slot, parent_slot, ctx) {
            Ok(()) => {
                slot_delay_start.stop();
                let _ = ctx
                    .slot_metrics
                    .slot_delay_hist
                    .increment(slot_delay_start.as_us())
                    .inspect_err(|e| {
                        error!(
                            "{}: unable to increment slot delay histogram {e:?}",
                            ctx.my_pubkey
                        );
                    });

                return Ok(());
            }
            Err(StartLeaderError::ReplayIsBehind(_, _)) => {
                trace!(
                    "{my_pubkey}: Attempting to produce slot {slot}, however replay of the the \
                     parent {parent_slot} is not yet finished, waiting. Skip timer {}",
                    skip_timer.elapsed().as_millis()
                );
                let highest_frozen_slot = ctx
                    .replay_highest_frozen
                    .highest_frozen_slot
                    .lock()
                    .unwrap();

                // We wait until either we finish replay of the parent or the skip timer finishes
                let mut wait_start = Measure::start("replay_is_behind");
                let _unused = ctx
                    .replay_highest_frozen
                    .freeze_notification
                    .wait_timeout_while(
                        highest_frozen_slot,
                        timeout.saturating_sub(skip_timer.elapsed()),
                        |hfs| *hfs < parent_slot,
                    )
                    .unwrap();
                wait_start.stop();
                ctx.slot_metrics.replay_is_behind_cumulative_wait_elapsed += wait_start.as_us();
                let _ = ctx
                    .slot_metrics
                    .replay_is_behind_wait_elapsed_hist
                    .increment(wait_start.as_us())
                    .inspect_err(|e| {
                        error!(
                            "{}: unable to increment replay is behind histogram {e:?}",
                            ctx.my_pubkey
                        );
                    });
            }
            Err(e) => return Err(e),
        }
    }

    trace!(
        "{my_pubkey}: Skipping production of {slot}: Unable to replay parent {parent_slot} in time"
    );
    Err(StartLeaderError::ReplayIsBehind(parent_slot, slot))
}

/// Checks if we are set to produce a leader block for `slot`:
/// - Is the highest notarization/finalized slot from `consensus_pool` frozen
/// - Startup verification is complete
/// - Bank forks does not already contain a bank for `slot`
///
/// If checks pass we return `Ok(())` and:
/// - Reset poh to the `parent_slot`
/// - Create a new bank for `slot` with parent `parent_slot`
/// - Insert into bank_forks and poh recorder
fn maybe_start_leader(
    slot: Slot,
    parent_slot: Slot,
    ctx: &mut LeaderContext,
) -> Result<(), StartLeaderError> {
    if ctx.bank_forks.read().unwrap().get(slot).is_some() {
        ctx.slot_metrics.already_have_bank_count += 1;
        return Err(StartLeaderError::AlreadyHaveBank(slot));
    }

    let Some(parent_bank) = ctx.bank_forks.read().unwrap().get(parent_slot) else {
        ctx.slot_metrics.replay_is_behind_count += 1;
        return Err(StartLeaderError::ReplayIsBehind(parent_slot, slot));
    };

    if !parent_bank.is_frozen() {
        ctx.slot_metrics.replay_is_behind_count += 1;
        return Err(StartLeaderError::ReplayIsBehind(parent_slot, slot));
    }

    // Create and insert the bank
    create_and_insert_leader_bank(slot, parent_bank, ctx);
    Ok(())
}

/// Creates and inserts the leader bank `slot` of this window with
/// parent `parent_bank`
fn create_and_insert_leader_bank(slot: Slot, parent_bank: Arc<Bank>, ctx: &mut LeaderContext) {
    let parent_slot = parent_bank.slot();
    let root_slot = ctx.bank_forks.read().unwrap().root();
    trace!(
        "{}: Creating and inserting leader slot {slot} parent {parent_slot} root {root_slot}",
        ctx.my_pubkey
    );

    if let Some(bank) = ctx.poh_recorder.read().unwrap().bank() {
        panic!(
            "{}: Attempting to produce a block for {slot}, however we still are in production of \
             {}. Something has gone wrong with the block creation loop. exiting",
            ctx.my_pubkey,
            bank.slot(),
        );
    }

    if ctx.poh_recorder.read().unwrap().start_slot() != parent_slot {
        // Important to keep Poh somewhat accurate for
        // parts of the system relying on PohRecorder::would_be_leader()
        reset_poh_recorder(&parent_bank, ctx);
    }

    let tpu_bank = ReplayStage::new_bank_from_parent_with_notify(
        parent_bank.clone(),
        slot,
        root_slot,
        &ctx.my_pubkey,
        ctx.rpc_subscriptions.as_deref(),
        &ctx.slot_status_notifier,
        NewBankOptions::default(),
    );
    // make sure parent is frozen for finalized hashes via the above
    // new()-ing of its child bank
    ctx.banking_tracer.hash_event(
        parent_slot,
        &parent_bank.last_blockhash(),
        &parent_bank.hash(),
    );

    // Insert the bank
    let tpu_bank = ctx.bank_forks.write().unwrap().insert(tpu_bank);
    ctx.poh_recorder.write().unwrap().set_bank(tpu_bank);
    ctx.record_receiver.restart(slot);
    ctx.slot_metrics.reset(slot);

    info!(
        "{}: new fork:{} parent:{} (leader) root:{}",
        ctx.my_pubkey, slot, parent_slot, root_slot
    );
}
