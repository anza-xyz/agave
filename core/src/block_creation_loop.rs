//! The block creation loop
//! When our leader window is reached, attempts to create our leader blocks
//! within the block timeouts. Responsible for inserting empty banks for
//! banking stage to fill, and clearing banks once the timeout has been reached.
//!
//! Once alpenglow is active, this is the only thread that will touch the [`PohRecorder`].
use {
    crate::{
        banking_trace::{BankingPacketSender, BankingTracer},
        replay_stage::{Finalizer, ReplayStage},
    },
    agave_votor::event::LeaderWindowInfo,
    agave_votor_messages::{
        consensus_message::Block,
        reward_certificate::{
            BuildRewardCertsRequest, BuildRewardCertsRespSucc, BuildRewardCertsResponse,
            NotarRewardCertificate, SkipRewardCertificate,
        },
    },
    crossbeam_channel::{Receiver, RecvTimeoutError, Sender, select_biased},
    solana_clock::Slot,
    solana_entry::block_component::{
        BlockFooterV1, GenesisCertificate, UpdateParentV1, VersionedBlockMarker,
    },
    solana_gossip::cluster_info::ClusterInfo,
    solana_hash::Hash,
    solana_ledger::{blockstore::Blockstore, leader_schedule_cache::LeaderScheduleCache},
    solana_measure::measure::Measure,
    solana_perf::packet::{BytesPacket, Meta, PacketBatch, bytes::Bytes},
    solana_poh::{
        poh_recorder::{GRACE_TICKS_FACTOR, MAX_GRACE_SLOTS, PohRecorder, PohRecorderError},
        record_channels::RecordReceiver,
    },
    solana_pubkey::Pubkey,
    solana_rpc::{rpc_subscriptions::RpcSubscriptions, slot_status_notifier::SlotStatusNotifier},
    solana_runtime::{
        bank::{Bank, NewBankOptions},
        bank_forks::BankForks,
        block_component_processor::BlockComponentProcessor,
        leader_schedule_utils::{last_of_consecutive_leader_slots, leader_slot_index},
        validated_block_finalization::ValidatedBlockFinalizationCert,
        validated_reward_certificate::ValidatedRewardCert,
    },
    solana_transaction::versioned::VersionedTransaction,
    solana_version::version,
    stats::{LoopMetrics, SlotMetrics},
    std::{
        sync::{
            Arc, Condvar, Mutex, RwLock,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    },
    thiserror::Error,
};

mod stats;

enum ParentSource {
    ParentReady(LeaderWindowInfo),
    OptimisticParent(LeaderWindowInfo),
}

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
    pub highest_finalized: Arc<RwLock<Option<ValidatedBlockFinalizationCert>>>,

    // Channel to receive RecordReceiver from PohService
    pub record_receiver_receiver: Receiver<RecordReceiver>,
    pub optimistic_parent_receiver: Receiver<LeaderWindowInfo>,

    /// Channel to send the request to build reward certs.
    pub build_reward_certs_sender: Sender<BuildRewardCertsRequest>,
    /// Channel to receive the built reward certs.
    pub reward_certs_receiver: Receiver<BuildRewardCertsResponse>,

    /// Sender for packets to banking stage (used to re-inject transactions after sad leader handover).
    pub banking_stage_sender: BankingPacketSender,
}

struct LeaderContext {
    exit: Arc<AtomicBool>,
    my_pubkey: Pubkey,
    leader_window_info_receiver: Receiver<LeaderWindowInfo>,
    highest_parent_ready: Arc<RwLock<(Slot, (Slot, Hash))>>,
    highest_finalized: Arc<RwLock<Option<ValidatedBlockFinalizationCert>>>,

    blockstore: Arc<Blockstore>,
    record_receiver: RecordReceiver,
    poh_recorder: Arc<RwLock<PohRecorder>>,
    leader_schedule_cache: Arc<LeaderScheduleCache>,
    bank_forks: Arc<RwLock<BankForks>>,
    rpc_subscriptions: Option<Arc<RpcSubscriptions>>,
    slot_status_notifier: Option<SlotStatusNotifier>,
    banking_tracer: Arc<BankingTracer>,
    replay_highest_frozen: Arc<ReplayHighestFrozen>,
    build_reward_certs_sender: Sender<BuildRewardCertsRequest>,
    reward_certs_receiver: Receiver<BuildRewardCertsResponse>,
    banking_stage_sender: BankingPacketSender,

    // Metrics
    metrics: LoopMetrics,
    slot_metrics: SlotMetrics,

    // Migration information
    genesis_cert: GenesisCertificate,
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
        record_receiver_receiver,
        leader_window_info_receiver,
        replay_highest_frozen,
        highest_parent_ready,
        optimistic_parent_receiver,
        build_reward_certs_sender,
        reward_certs_receiver,
        highest_finalized,
        banking_stage_sender,
    } = config;

    // Similar to Votor, if this loop dies kill the validator
    let _exit = Finalizer::new(exit.clone());

    // get latest identity pubkey during startup
    let mut my_pubkey = cluster_info.id();

    info!("{my_pubkey}: Block creation loop initialized");

    // Wait for PohService to be shutdown
    let record_receiver = match record_receiver_receiver.recv() {
        Ok(receiver) => receiver,
        Err(e) => {
            info!("{my_pubkey}: Failed to receive RecordReceiver from PohService. Exiting: {e:?}",);
            return;
        }
    };

    let genesis_cert = bank_forks
        .read()
        .unwrap()
        .migration_status()
        .genesis_certificate()
        .expect("Migration complete, genesis certificate must exist");
    let genesis_cert = GenesisCertificate::try_from((*genesis_cert).clone())
        .expect("Genesis certificate must be valid");

    info!("{my_pubkey}: PohService has shutdown, BlockCreationLoop is enabled");

    let mut ctx = LeaderContext {
        exit,
        my_pubkey,
        highest_parent_ready,
        leader_window_info_receiver,
        blockstore,
        poh_recorder: poh_recorder.clone(),
        record_receiver,
        leader_schedule_cache,
        bank_forks,
        rpc_subscriptions,
        slot_status_notifier,
        banking_tracer,
        replay_highest_frozen,
        build_reward_certs_sender,
        reward_certs_receiver,
        banking_stage_sender,
        metrics: LoopMetrics::default(),
        slot_metrics: SlotMetrics::default(),
        highest_finalized,
        genesis_cert,
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

        // Wait for the first window notification, then drain both sources and pick the newest
        // leader window. This avoids revisiting stale optimistic windows after replay has already
        // advanced to a later finalized parent.
        let window_source = select_biased! {
            recv(ctx.leader_window_info_receiver) -> msg => {
                msg.ok().map(ParentSource::ParentReady)
            },
            recv(optimistic_parent_receiver) -> msg => {
                msg.ok().map(ParentSource::OptimisticParent)
            },
            default(Duration::from_secs(1)) => continue,
        };

        let (mut latest_parent_ready, mut latest_optimistic_parent) = match window_source {
            Some(ParentSource::ParentReady(first)) => (Some(first), None),
            Some(ParentSource::OptimisticParent(first)) => (None, Some(first)),
            None => {
                info!("{my_pubkey}: channel disconnected");
                return;
            }
        };

        latest_parent_ready = ctx
            .leader_window_info_receiver
            .try_iter()
            .last()
            .or(latest_parent_ready);
        latest_optimistic_parent = optimistic_parent_receiver
            .try_iter()
            .last()
            .or(latest_optimistic_parent);

        let (info, fast_leader_handover) = match (latest_parent_ready, latest_optimistic_parent) {
            (Some(parent_ready), Some(optimistic_parent))
                if optimistic_parent.start_slot > parent_ready.start_slot =>
            {
                trace!(
                    "{my_pubkey}: picked newer optimistic window {}-{} over finalized {}-{}",
                    optimistic_parent.start_slot,
                    optimistic_parent.end_slot,
                    parent_ready.start_slot,
                    parent_ready.end_slot,
                );
                (optimistic_parent, true)
            }
            (Some(parent_ready), Some(optimistic_parent)) => {
                if parent_ready.start_slot > optimistic_parent.start_slot {
                    trace!(
                        "{my_pubkey}: picked newer finalized window {}-{} over optimistic {}-{}",
                        parent_ready.start_slot,
                        parent_ready.end_slot,
                        optimistic_parent.start_slot,
                        optimistic_parent.end_slot,
                    );
                } else {
                    trace!(
                        "{my_pubkey}: both finalized and optimistic parents ready for window \
                         {}-{}, preferring finalized parent",
                        parent_ready.start_slot, parent_ready.end_slot,
                    );
                }
                (parent_ready, false)
            }
            (Some(info), None) => {
                trace!(
                    "{my_pubkey}: window {}-{} finalized parent",
                    info.start_slot, info.end_slot
                );
                (info, false)
            }
            (None, Some(info)) => {
                trace!(
                    "{my_pubkey}: window {}-{} optimistic parent",
                    info.start_slot, info.end_slot
                );
                (info, true)
            }
            (None, None) => {
                info!("{my_pubkey}: both leader window channels drained");
                continue;
            }
        };

        let LeaderWindowInfo {
            start_slot,
            end_slot,
            parent_block: (parent_slot, parent_hash),
            block_timer,
        } = info;

        trace!(
            "{my_pubkey}: window {start_slot}-{end_slot} parent {parent_slot} \
             flh={fast_leader_handover}"
        );

        if (start_slot..=end_slot).any(|slot| ctx.blockstore.has_existing_shreds_for_slot(slot)) {
            warn!("{my_pubkey}: already have shreds in window {start_slot}-{end_slot}, skipping");
            continue;
        }

        if let Err(e) = produce_window(
            fast_leader_handover,
            start_slot,
            end_slot,
            parent_slot,
            parent_hash,
            block_timer,
            &mut ctx,
        ) {
            // Give up on this leader window
            error!(
                "{my_pubkey}: Unable to produce window {start_slot}-{end_slot}, skipping window: \
                 {e:?}"
            );
            continue;
        }

        ctx.metrics.loop_count += 1;
        ctx.metrics.report(Duration::from_millis(1000));
    }

    info!("{my_pubkey}: Block creation loop shutting down");
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

/// From the passed in bank (used for determining slot times) and leader block
/// index within the leader window, returns the duration after which we should
/// publish the final shred for the block with starting point being the start of
/// the leader window.
fn block_timeout(bank: &Bank, leader_block_index: usize) -> Duration {
    Duration::from_nanos_u128(bank.ns_per_slot)
        .saturating_mul((leader_block_index as u32).saturating_add(1))
}

/// Clamps the block producer timestamp to ensure that the leader produces a timestamp that conforms
/// to Alpenglow clock bounds.
fn skew_block_producer_time_nanos(
    parent_slot: Slot,
    parent_time_nanos: i64,
    working_bank_slot: Slot,
    working_bank_time_nanos: i64,
    ns_per_slot: u64,
) -> i64 {
    let (min_working_bank_time, max_working_bank_time) =
        BlockComponentProcessor::nanosecond_time_bounds(
            parent_slot,
            parent_time_nanos,
            working_bank_slot,
            ns_per_slot,
        );

    working_bank_time_nanos
        .max(min_working_bank_time)
        .min(max_working_bank_time)
}

/// Produces a block footer with the current timestamp; version; reward certs; and finalization cert.
/// The bank_hash field is left as default and will be filled in after the bank freezes.
fn produce_block_footer(
    bank: &Bank,
    skip_reward_cert: Option<SkipRewardCertificate>,
    notar_reward_cert: Option<NotarRewardCertificate>,
    highest_finalized: Option<&ValidatedBlockFinalizationCert>,
) -> BlockFooterV1 {
    let mut block_producer_time_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Misconfigured system clock; couldn't measure block producer time.")
        .as_nanos() as i64;

    let slot = bank.slot();

    if let Some(parent_bank) = bank.parent() {
        // Get parent time from alpenglow clock (nanoseconds) or fall back to clock sysvar (seconds -> nanoseconds)
        let parent_time_nanos = bank
            .get_nanosecond_clock()
            .unwrap_or_else(|| bank.clock().unix_timestamp.saturating_mul(1_000_000_000));
        let parent_slot = parent_bank.slot();
        let ns_per_slot = u64::try_from(bank.ns_per_slot).unwrap_or(u64::MAX);

        block_producer_time_nanos = skew_block_producer_time_nanos(
            parent_slot,
            parent_time_nanos,
            slot,
            block_producer_time_nanos,
            ns_per_slot,
        );
    }

    // Convert finalization certs into block marker
    let final_cert = highest_finalized.map(ValidatedBlockFinalizationCert::to_final_certificate);

    BlockFooterV1 {
        bank_hash: Hash::default(),
        block_producer_time_nanos: block_producer_time_nanos as u64,
        block_user_agent: format!("agave/{}", version!()).into_bytes(),
        final_cert,
        skip_reward_cert,
        notar_reward_cert,
    }
}

/// Produces the leader window from `start_slot` -> `end_slot` using parent
/// `parent_slot` while abiding to the `block_timer`
fn produce_window(
    fast_leader_handover: bool,
    start_slot: Slot,
    end_slot: Slot,
    parent_slot: Slot,
    parent_hash: Hash,
    mut block_timer: Instant,
    ctx: &mut LeaderContext,
) -> Result<(), StartLeaderError> {
    // Insert the first bank
    let mut working_bank =
        start_leader_wait_for_parent_replay(start_slot, parent_slot, block_timer, ctx)?;
    if fast_leader_handover {
        ctx.slot_metrics.mark_fast_leader_handover();
    }

    let my_pubkey = ctx.my_pubkey;
    let mut window_production_start = Measure::start("window_production");
    let mut slot = start_slot;

    while !ctx.exit.load(Ordering::Relaxed) && slot <= end_slot {
        let timeout = block_timeout(&working_bank, leader_slot_index(slot));
        trace!(
            "{my_pubkey}: waiting for leader bank {slot} to finish, remaining time: {}ms",
            timeout.saturating_sub(block_timer.elapsed()).as_millis()
        );

        let mut bank_completion_measure = Measure::start("bank_completion");
        let optimistic_parent =
            (fast_leader_handover && slot == start_slot).then_some((parent_slot, parent_hash));
        if let Err(e) =
            record_and_complete_block(ctx, slot, optimistic_parent, &mut block_timer, timeout)
        {
            if ctx.exit.load(Ordering::Relaxed) {
                return Ok(());
            }
            panic!("PohRecorder record failed: {e:?}");
        }
        assert!(!ctx.poh_recorder.read().unwrap().has_bank());
        bank_completion_measure.stop();
        ctx.slot_metrics.report();

        ctx.metrics.bank_timeout_completion_count += 1;
        let _ = ctx
            .metrics
            .bank_timeout_completion_elapsed_hist
            .increment(bank_completion_measure.as_us());

        // Produce our next slot
        slot += 1;
        if slot > end_slot {
            trace!("{my_pubkey}: finished leader window {start_slot}-{end_slot}");
            break;
        }

        // Although `slot - 1`has been cleared from `poh_recorder`, it might not have finished processing in
        // `replay_stage`, which is why we use `start_leader_retry_replay`
        working_bank = start_leader_wait_for_parent_replay(slot, slot - 1, block_timer, ctx)?;
    }

    window_production_start.stop();
    ctx.metrics.window_production_elapsed += window_production_start.as_us();
    Ok(())
}

/// Records incoming transactions until we reach the block timeout.
/// Afterwards:
/// - Shutdown the record receiver
/// - Clear any inflight records
/// - Insert the block footer
/// - Insert the alpentick
/// - Clear the working bank
fn record_and_complete_block(
    ctx: &mut LeaderContext,
    bank_slot: Slot,
    mut optimistic_parent: Option<(Slot, Hash)>,
    block_timer: &mut Instant,
    block_timeout: Duration,
) -> Result<(), PohRecorderError> {
    ctx.build_reward_certs_sender
        .send(BuildRewardCertsRequest { bank_slot })
        .map_err(|_| PohRecorderError::ChannelDisconnected)?;
    let mut accumulated_txs = vec![];
    let window_has_moved_on = loop {
        if ctx.exit.load(Ordering::Relaxed) {
            return Err(PohRecorderError::ChannelDisconnected);
        }

        // Don't timeout until we've received ParentReady.
        let block_time_left = time_left(*block_timer, block_timeout);
        if block_time_left.is_zero() && optimistic_parent.is_none() {
            break false;
        }

        // If our window is skipped return
        if ctx.highest_parent_ready.read().unwrap().0 > bank_slot {
            break true;
        }

        // Once the block timeout has elapsed, fast leader handover can still need to wait for
        // ParentReady. Poll periodically so shutdown can be observed without a zero-duration spin.
        let select_timeout = if block_time_left.is_zero() {
            Duration::from_millis(100)
        } else {
            block_time_left
        };

        select_biased! {
            recv(ctx.leader_window_info_receiver) -> msg => {
                match msg {
                    Ok(info) if info.start_slot > bank_slot => {
                        // Window has moved on; we're behind
                        break true;
                    }
                    Ok(info) => {
                        if let Some(optimistic_parent_block) = optimistic_parent.take() {
                            handle_parent_ready(
                                ctx,
                                info,
                                optimistic_parent_block,
                                std::mem::take(&mut accumulated_txs),
                                block_timer,
                            )?;
                        }
                    }
                    Err(_) => return Err(PohRecorderError::ChannelDisconnected),
                }
            },
            recv(ctx.record_receiver.inner()) -> msg => {
                let record = msg.map_err(|_| PohRecorderError::ChannelDisconnected)?;
                ctx.record_receiver
                    .on_received_record(record.transaction_batches.len() as u64);

                if optimistic_parent.is_some() {
                    record.transaction_batches.iter().for_each(|batch| {
                        accumulated_txs.extend(batch.iter().cloned());
                    });
                }

                ctx.poh_recorder.write().unwrap().record(
                    record.bank_id,
                    record.mixins,
                    record.transaction_batches,
                )?;
            },
            default(select_timeout) => {},
        }
    };

    // Shutdown and clear any inflight records
    shutdown_and_drain_record_receiver(&ctx.poh_recorder, &mut ctx.record_receiver)?;

    // By now, we should have received ParentReady and called handle_parent_ready(), unless
    // we're behind and the window has moved on.
    debug_assert!(
        window_has_moved_on || optimistic_parent.is_none(),
        "optimistic_parent should be None after receiving ParentReady"
    );

    // Alpentick and clear bank
    let mut w_poh_recorder = ctx.poh_recorder.write().unwrap();
    let bank = w_poh_recorder
        .bank()
        .expect("Bank cannot have been cleared as BlockCreationLoop is the only modifier");

    trace!(
        "{}: bank {} has reached block timeout, ticking",
        bank.leader_id(),
        bank.slot()
    );

    let max_tick_height = bank.max_tick_height();
    // Set the tick height for the bank to max_tick_height - 1, so that PohRecorder::flush_cache()
    // will properly increment the tick_height to max_tick_height.
    bank.set_tick_height(max_tick_height - 1);

    let footer = {
        let reward_certs = loop {
            if ctx.exit.load(Ordering::Relaxed) {
                return Err(PohRecorderError::ChannelDisconnected);
            }

            match ctx
                .reward_certs_receiver
                .recv_timeout(Duration::from_millis(100))
            {
                Ok(reward_certs) => break reward_certs?,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(PohRecorderError::ChannelDisconnected);
                }
            }
        };
        let BuildRewardCertsRespSucc {
            skip,
            notar,
            validators: _,
        } = reward_certs;
        let reward_cert = ValidatedRewardCert::try_new(&bank, &skip, &notar)?;
        let guard = ctx.highest_finalized.read().unwrap();
        let footer = produce_block_footer(&bank, skip, notar, guard.as_ref());
        let final_cert_input = guard.as_ref().map(|c| c.vote_rewards_input());

        BlockComponentProcessor::update_bank_with_footer_fields(
            &bank,
            footer.block_producer_time_nanos as i64,
            Hash::default(), // Banks we produce do not need the bank hash mismatch check
            reward_cert,
            final_cert_input,
        );
        footer
    };

    drop(bank);
    // Write the single tick for this slot
    w_poh_recorder.tick_alpenglow(max_tick_height, footer);
    Ok(())
}

fn send_update_parent(
    poh_recorder: &RwLock<PohRecorder>,
    new_parent_block: Block,
) -> Result<(), PohRecorderError> {
    let (new_parent_slot, new_parent_block_id) = new_parent_block;
    let update_parent = UpdateParentV1 {
        new_parent_slot,
        new_parent_block_id,
    };
    let marker = VersionedBlockMarker::new_update_parent(update_parent);
    poh_recorder.write().unwrap().send_marker(marker)?;
    Ok(())
}

/// Handles a parent ready notification when building on an optimistic parent.
///
/// Happy path:
/// - finalized parent matches optimistic parent
/// - this is a no-op, and a win.
///
/// Sad path:
/// - finalized parent does not match optimistic parent
/// - we send an UpdateParent to switch to the correct parent (i.e., the finalized parent).
/// - tell PohRecorder to stop sending us transactions
/// - we clear the bank for the current slot
fn handle_parent_ready(
    ctx: &mut LeaderContext,
    leader_window_info: LeaderWindowInfo,
    optimistic_parent_block: (Slot, Hash),
    accumulated_txs: Vec<VersionedTransaction>,
    block_timer: &mut Instant,
) -> Result<Option<Arc<Bank>>, PohRecorderError> {
    if leader_window_info.parent_block == optimistic_parent_block {
        // Happy path: optimistic parent matches finalized parent
        return Ok(None);
    }

    // Sad path: need to switch to the correct parent
    trace!(
        "{:?}: Sad leader handover slot optimistic parent = {:?} != {:?} = finalized parent",
        ctx.my_pubkey, optimistic_parent_block, leader_window_info.parent_block
    );

    // If the optimistic parent doesn't match the finalized parent (specified in ParentReady), then
    // this resets the block timer to the new parent's timer.
    *block_timer = leader_window_info.block_timer;

    // Important: We must shutdown and drain the record receiver BEFORE sending the UpdateParent
    // marker. Otherwise, we could end up sending records for the old bank after the UpdateParent,
    // which causes a divergence between the leader and replayers.
    shutdown_and_drain_record_receiver(&ctx.poh_recorder, &mut ctx.record_receiver)?;
    send_update_parent(&ctx.poh_recorder, leader_window_info.parent_block)?;

    let slot = leader_window_info.start_slot;
    let (old_parent_slot, _) = optimistic_parent_block;
    let (new_parent_slot, _) = leader_window_info.parent_block;

    ctx.bank_forks.write().unwrap().clear_bank(slot, false);

    // Re-inject accumulated transactions back to banking stage for rescheduling
    let packets: Vec<BytesPacket> = accumulated_txs
        .into_iter()
        .filter_map(|tx| {
            // TODO(ksn): use wincode once we upstream to Agave
            let serialized = bincode::serialize(&tx)
                .inspect_err(|e| {
                    error!(
                        "failed to serialize transaction for rescheduling - this should never \
                         happen: {e:?}"
                    )
                })
                .ok()?;
            let buffer = Bytes::from(serialized);
            let mut meta = Meta::default();
            meta.size = buffer.len();
            Some(BytesPacket::new(buffer, meta))
        })
        .collect();

    if !packets.is_empty() {
        info!(
            "{}: rescheduling {} txs after sad leader handover for slot {slot}",
            ctx.my_pubkey,
            packets.len(),
        );
        let batch: PacketBatch = packets.into();
        let banking_packet_batch = Arc::new(vec![batch]);
        ctx.banking_stage_sender
            .send(banking_packet_batch)
            .map_err(|_| PohRecorderError::RescheduleTransactionsError(slot))?;
    }

    // Wait for new parent to be frozen
    start_leader_wait_for_parent_replay(slot, new_parent_slot, *block_timer, ctx)
        .map(Option::Some)
        .map_err(|_| PohRecorderError::ResetBankError(old_parent_slot, new_parent_slot))
}

/// Shutdowns the record receiver and drains any remaining records.
fn shutdown_and_drain_record_receiver(
    poh_recorder: &RwLock<PohRecorder>,
    record_receiver: &mut RecordReceiver,
) -> Result<(), PohRecorderError> {
    record_receiver.shutdown();

    for record in record_receiver.drain() {
        poh_recorder.write().unwrap().record(
            record.bank_id,
            record.mixins,
            record.transaction_batches,
        )?;
    }

    Ok(())
}

/// Returns the time remaining until timeout.
fn time_left(block_timer: Instant, timeout: Duration) -> Duration {
    timeout.saturating_sub(block_timer.elapsed())
}

/// Similar to `maybe_start_leader`, however if replay of the parent block is lagging we retry
/// until either replay finishes or we hit the block timeout.
fn start_leader_wait_for_parent_replay(
    slot: Slot,
    parent_slot: Slot,
    block_timer: Instant,
    ctx: &mut LeaderContext,
) -> Result<Arc<Bank>, StartLeaderError> {
    trace!(
        "{}: Attempting to start leader slot {slot} parent {parent_slot}",
        ctx.my_pubkey
    );
    let my_pubkey = ctx.my_pubkey;
    let timeout = block_timeout(
        &ctx.bank_forks.read().unwrap().root_bank(),
        leader_slot_index(slot),
    );
    let end_slot = last_of_consecutive_leader_slots(slot);

    let mut slot_delay_start = Measure::start("slot_delay");
    while !time_left(block_timer, timeout).is_zero() {
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

                ctx.slot_metrics.report();
                return Ok(ctx
                    .poh_recorder
                    .read()
                    .unwrap()
                    .bank()
                    .expect("We just started the leader, so the bank must exist"));
            }
            Err(StartLeaderError::ReplayIsBehind(_, _)) => {
                trace!(
                    "{my_pubkey}: Attempting to produce slot {slot}, however replay of the parent \
                     {parent_slot} is not yet finished, waiting. Block timer {}",
                    block_timer.elapsed().as_millis()
                );
                let highest_frozen_slot = ctx
                    .replay_highest_frozen
                    .highest_frozen_slot
                    .lock()
                    .unwrap();

                // We wait until either we finish replay of the parent or the block timer finishes
                let mut wait_start = Measure::start("replay_is_behind");
                let _unused = {
                    let timeout = time_left(block_timer, timeout);
                    ctx.replay_highest_frozen
                        .freeze_notification
                        .wait_timeout_while(highest_frozen_slot, timeout, |hfs| *hfs < parent_slot)
                        .unwrap()
                };
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

    let Some(leader) = ctx
        .leader_schedule_cache
        .slot_leader_at(slot, Some(&parent_bank))
    else {
        panic!(
            "{}: No leader found for slot {slot} with parent {parent_slot}. Something has gone \
             wrong with the block creation loop. exiting",
            ctx.my_pubkey,
        );
    };

    if ctx.my_pubkey != leader.id {
        panic!(
            "{}: Attempting to produce a block for {slot}, however the leader is {}. Something \
             has gone wrong with the block creation loop. exiting",
            ctx.my_pubkey, leader.id,
        );
    }

    if ctx.poh_recorder.read().unwrap().start_slot() != parent_slot {
        // Important to keep Poh somewhat accurate for
        // parts of the system relying on PohRecorder::would_be_leader()
        reset_poh_recorder(&parent_bank, ctx);
    }

    // After potentially resetting, there should be no working bank.
    // If there still is one, something has gone wrong.
    if let Some(bank) = ctx.poh_recorder.read().unwrap().bank() {
        panic!(
            "{}: Attempting to produce a block for {slot}, however we still are in production of \
             {}. Something has gone wrong with the block creation loop. exiting",
            ctx.my_pubkey,
            bank.slot(),
        );
    }

    let tpu_bank = ReplayStage::new_bank_from_parent_with_notify(
        parent_bank.clone(),
        slot,
        root_slot,
        leader,
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
    let bank_id = tpu_bank.bank_id();
    ctx.poh_recorder.write().unwrap().set_bank(tpu_bank);

    // If this is the first alpenglow block, emit the genesis certificate marker
    maybe_include_genesis_certificate(parent_slot, ctx);

    // Wakeup banking stage
    ctx.record_receiver.restart(bank_id);
    ctx.slot_metrics.reset(slot);

    info!(
        "{}: new fork:{} parent:{} (leader) root:{}",
        ctx.my_pubkey, slot, parent_slot, root_slot
    );
}

///  If this the very first alpenglow block, include the genesis certificate
///  Note: if the alpenglow genesis is 0, then this is a test cluster with Alpenglow enabled
///  by default. No need to put in the genesis marker as the genesis account is already populated
///  during cluster creation.
fn maybe_include_genesis_certificate(parent_slot: Slot, ctx: &LeaderContext) {
    if parent_slot != ctx.genesis_cert.slot || parent_slot == 0 {
        return;
    }

    // Send the genesis certificate
    let genesis_marker = VersionedBlockMarker::new_genesis_certificate(ctx.genesis_cert.clone());
    let mut poh_recorder = ctx.poh_recorder.write().unwrap();
    poh_recorder
        .send_marker(genesis_marker)
        .expect("Max tick height cannot have been reached");

    // Process the genesis certificate
    let bank = poh_recorder.bank().expect("Bank cannot have been cleared");
    let processor = bank.block_component_processor.read().unwrap();
    processor
        .on_genesis_certificate(
            bank.clone(),
            ctx.genesis_cert.clone(),
            &ctx.bank_forks.read().unwrap().migration_status(),
        )
        .expect("Recording genesis certificate should not fail");
}
