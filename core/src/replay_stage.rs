//! The `replay_stage` replays transactions broadcast by the leader.
use {
    crate::{
        block_creation_loop::ReplayHighestFrozen,
        consensus::progress_map::{ForkProgress, ProgressMap},
        cost_update_service::CostUpdate,
    },
    agave_jemalloc::jemalloc::Arena,
    agave_votor::{
        commitment::{self, CommitmentAggregationData, CommitmentType},
        event::{
            CompletedBlock, LatestSwitchRequest, LeaderWindowInfo, SwitchBankEvent, VotorEvent,
            VotorEventSender,
        },
        root_utils,
    },
    agave_votor_messages::{
        certificate::Certificate,
        consensus_message::{Block, BlockId},
    },
    crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, select},
    itertools::Itertools,
    rayon::{ThreadPool, prelude::*},
    smallvec::SmallVec,
    solana_clock::Slot,
    solana_geyser_plugin_manager::block_metadata_notifier_interface::BlockMetadataNotifierArc,
    solana_gossip::cluster_info::ClusterInfo,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_leader_schedule::{NUM_CONSECUTIVE_LEADER_SLOTS, SlotLeader},
    solana_ledger::{
        blockstore::{Blockstore, BlockstoreError, UpdateParentReceiver},
        blockstore_meta::BlockLocation,
        blockstore_processor::{
            self, AsyncVerificationProgress, BlockstoreProcessorError, ConfirmationProgress,
            ExecuteBatchesInternalMetrics, ReplaySlotStats, ReplayVerificationWorkerPool,
        },
        entry_notifier_service::EntryNotifierSender,
        leader_schedule_cache::LeaderScheduleCache,
    },
    solana_measure::measure::Measure,
    solana_pubkey::Pubkey,
    solana_rpc::{
        optimistically_confirmed_bank_tracker::{BankNotification, BankNotificationSenderConfig},
        rpc_subscriptions::RpcSubscriptions,
        slot_status_notifier::SlotStatusNotifier,
    },
    solana_runtime::{
        bank::{Bank, MAX_ALPENGLOW_VOTE_ACCOUNTS},
        bank_forks::BankForks,
        bank_forks_controller::{BankForksCommand, BankForksCommandReceiver, SetRootCommand},
        block_component_processor::BlockComponentProcessorError,
        installed_scheduler_pool::BankWithScheduler,
        leader_schedule_utils::first_of_consecutive_leader_slots,
        snapshot_controller::SnapshotController,
        transaction_execution::TransactionStatusSender,
    },
    solana_time_utils::timestamp,
    std::{
        collections::{BTreeSet, HashMap, HashSet},
        num::{NonZeroUsize, Saturating},
        result,
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

mod dead_slots;
mod update_parent;

use {
    dead_slots::{DeadSlotContext, DeadSlotNotifications, mark_replay_dead_slot},
    update_parent::{
        ChildBankReplayStart, child_bank_replay_start, handle_abandoned_bank,
        handle_update_parent_interrupts, process_soft_dead_slots,
    },
};

const ASYNC_VERIFICATION_FREELIST_CAPACITY: usize = 5;

const VAT_STATUS_CHECK_INTERVAL_SECS: u64 = 30;

enum ForkReplayMode {
    Serial,
    Parallel(ThreadPool),
}

// Implement a destructor for the ReplayStage thread to signal it exited
// even on panics
pub(crate) struct Finalizer {
    exit_sender: Arc<AtomicBool>,
}

impl Finalizer {
    pub(crate) fn new(exit_sender: Arc<AtomicBool>) -> Self {
        Finalizer { exit_sender }
    }
}

// Implement a destructor for Finalizer.
impl Drop for Finalizer {
    fn drop(&mut self) {
        self.exit_sender.clone().store(true, Ordering::Relaxed);
    }
}

struct ReplaySlotFromBlockstore {
    is_slot_dead: bool,
    bank_slot: Slot,
    replay_result: Option<Result<usize /* tx count */, BlockstoreProcessorError>>,
}

impl ReplaySlotFromBlockstore {
    fn new(bank_slot: Slot) -> Self {
        Self {
            is_slot_dead: false,
            bank_slot,
            replay_result: None,
        }
    }
}

/// Select the freshest leader-window notification by start slot, replacing equal
/// start slots with the later notification. This intentionally mirrors BCL's
/// `freshest_window_from_iter` selection logic.
fn freshest_leader_window(
    current: LeaderWindowInfo,
    candidate: LeaderWindowInfo,
) -> LeaderWindowInfo {
    if current.start_slot > candidate.start_slot {
        current
    } else {
        candidate
    }
}

struct BankReplayTracker {
    // The bank being replayed.
    bank: BankWithScheduler,
    // Metrics around replaying this bank.
    replay_stats: Arc<RwLock<ReplaySlotStats>>,
    // Accounting around replaying this bank.
    replay_progress: Arc<RwLock<ConfirmationProgress>>,
}

struct BankReplayResultTracker {
    // The most recent result from replaying the slot.
    replay_result: ReplaySlotFromBlockstore,
    // Will be Some if bank is ready for replay. None implies this fork was
    // marked dead or is unrooted.
    bank_replay_tracker: Option<BankReplayTracker>,
}

enum SchedulerReplayOutcome {
    NoSchedulerReplay,
    SchedulerReplayCompleted(Result<(), BlockstoreProcessorError>),
}

struct CompletedBankReplay {
    // Metrics around replaying this bank.
    replay_stats: Arc<RwLock<ReplaySlotStats>>,
    // Accounting around replaying this bank.
    replay_progress: Arc<RwLock<ConfirmationProgress>>,
    // True when the bank completed via the unified scheduler path.
    is_unified_scheduler_enabled: bool,
}

struct ProcessActiveBanksContext {
    bank_forks: Arc<RwLock<BankForks>>,
    blockstore: Arc<Blockstore>,
    transaction_status_sender: Option<TransactionStatusSender>,
    entry_notification_sender: Option<EntryNotifierSender>,
    bank_notification_sender: Option<BankNotificationSenderConfig>,
    rpc_subscriptions: Option<Arc<RpcSubscriptions>>,
    slot_status_notifier: Option<SlotStatusNotifier>,
    cost_update_sender: Sender<CostUpdate>,
    block_metadata_notifier: Option<BlockMetadataNotifierArc>,
    votor_event_sender: VotorEventSender,
    replay_mode: ForkReplayMode,
    replay_verification_worker_pool: ReplayVerificationWorkerPool,
}

struct ProcessBankForksContext {
    bank_forks: Arc<RwLock<BankForks>>,
    blockstore: Arc<Blockstore>,
    snapshot_controller: Option<Arc<SnapshotController>>,
    bank_notification_sender: Option<BankNotificationSenderConfig>,
    rpc_subscriptions: Option<Arc<RpcSubscriptions>>,
    drop_bank_sender: Sender<Vec<BankWithScheduler>>,
    leader_schedule_cache: Arc<LeaderScheduleCache>,
    votor_commitment_sender: Sender<CommitmentAggregationData>,
}

impl ProcessActiveBanksContext {
    /// Clone the handles needed to publish a dead-slot transition.
    fn dead_slot_notifications(&self) -> DeadSlotNotifications {
        DeadSlotNotifications {
            blockstore: self.blockstore.clone(),
            rpc_subscriptions: self.rpc_subscriptions.clone(),
            slot_status_notifier: self.slot_status_notifier.clone(),
        }
    }

    /// Build the full context needed when a replay error must become a hard or
    /// soft dead-slot transition.
    fn dead_slot_context(&self) -> DeadSlotContext {
        DeadSlotContext {
            notifications: self.dead_slot_notifications(),
        }
    }
}

/// Borrowed inputs that do not change while discovering new replay banks.
struct NewBankForksContext<'a> {
    /// Ledger data and SlotMeta used to discover children of frozen banks.
    blockstore: &'a Blockstore,
    /// Fork graph where new banks are inserted after discovery.
    bank_forks: &'a RwLock<BankForks>,
    /// Leader schedule used to construct child banks.
    leader_schedule_cache: &'a LeaderScheduleCache,
    /// Optional RPC fanout for bank-created notifications.
    rpc_subscriptions: Option<&'a RpcSubscriptions>,
    /// Optional slot-status fanout for bank-created notifications.
    slot_status_notifier: &'a Option<SlotStatusNotifier>,
    /// This validator's identity, used to leave live own-leader banks to BCL.
    my_pubkey: &'a Pubkey,
}

pub struct ReplayStageConfig {
    pub vote_account: Pubkey,
    pub authorized_voter_keypairs: Arc<RwLock<Vec<Arc<Keypair>>>>,
    pub exit: Arc<AtomicBool>,
    pub leader_schedule_cache: Arc<LeaderScheduleCache>,
    pub replay_forks_threads: NonZeroUsize,
    pub replay_transactions_threads: NonZeroUsize,
    pub blockstore: Arc<Blockstore>,
    pub bank_forks: Arc<RwLock<BankForks>>,
    pub cluster_info: Arc<ClusterInfo>,
    pub snapshot_controller: Option<Arc<SnapshotController>>,
    pub replay_highest_frozen: Arc<ReplayHighestFrozen>,
    pub highest_parent_ready: Arc<RwLock<(Slot, Block)>>,
    pub(crate) replay_arena: Option<Arena>,
}

pub struct ReplaySenders {
    pub rpc_subscriptions: Option<Arc<RpcSubscriptions>>,
    pub slot_status_notifier: Option<SlotStatusNotifier>,
    pub transaction_status_sender: Option<TransactionStatusSender>,
    pub entry_notification_sender: Option<EntryNotifierSender>,
    pub bank_notification_sender: Option<BankNotificationSenderConfig>,
    pub cost_update_sender: Sender<CostUpdate>,
    pub drop_bank_sender: Sender<Vec<BankWithScheduler>>,
    pub block_metadata_notifier: Option<BlockMetadataNotifierArc>,
    pub votor_event_sender: VotorEventSender,
    pub footer_certs_sender: Sender<SmallVec<[Certificate; 2]>>,
    pub optimistic_parent_sender: Sender<LeaderWindowInfo>,
    pub votor_commitment_sender: Sender<CommitmentAggregationData>,
}

pub struct ReplayReceivers {
    pub ledger_signal_receiver: Receiver<bool>,
    pub update_parent_receiver: UpdateParentReceiver,
    pub optimistic_parent_receiver: Receiver<LeaderWindowInfo>,
    pub bank_forks_controller_receiver: BankForksCommandReceiver,
    pub latest_switch_request: LatestSwitchRequest,
}

/// Timing information for the ReplayStage main processing loop
#[derive(Default)]
struct ReplayLoopTiming {
    last_submit: u64,
    loop_count: u64,
    generate_new_bank_forks_elapsed_us: u64,
    replay_active_banks_elapsed_us: u64,
    wait_receive_elapsed_us: u64,
    process_switch_bank_events_elapsed_us: u64,
    generate_new_bank_forks_read_lock_us: Saturating<u64>,
    generate_new_bank_forks_get_slots_since_us: Saturating<u64>,
    generate_new_bank_forks_loop_us: Saturating<u64>,
    generate_new_bank_forks_write_lock_us: Saturating<u64>,
    // When processing multiple forks concurrently, only captures the longest fork.
    replay_blockstore_us: u64,
}

impl ReplayLoopTiming {
    fn update(
        &mut self,
        generate_new_bank_forks_elapsed_us: u64,
        replay_active_banks_elapsed_us: u64,
        wait_receive_elapsed_us: u64,
    ) {
        self.loop_count += 1;
        self.generate_new_bank_forks_elapsed_us += generate_new_bank_forks_elapsed_us;
        self.replay_active_banks_elapsed_us += replay_active_banks_elapsed_us;
        self.wait_receive_elapsed_us += wait_receive_elapsed_us;
        self.maybe_submit();
    }

    fn maybe_submit(&mut self) {
        let now = timestamp();
        let elapsed_ms = now - self.last_submit;
        if elapsed_ms <= 1000 {
            return;
        }
        let &mut Self {
            generate_new_bank_forks_read_lock_us: Saturating(read_lock_us),
            generate_new_bank_forks_get_slots_since_us: Saturating(get_slots_since_us),
            generate_new_bank_forks_loop_us: Saturating(forks_loop_us),
            generate_new_bank_forks_write_lock_us: Saturating(write_lock_us),
            ..
        } = self;
        datapoint_info!(
            "replay-loop-timing-stats",
            ("loop_count", self.loop_count as i64, i64),
            ("total_elapsed_us", elapsed_ms * 1000, i64),
            (
                "generate_new_bank_forks_elapsed_us",
                self.generate_new_bank_forks_elapsed_us as i64,
                i64
            ),
            (
                "replay_active_banks_elapsed_us",
                self.replay_active_banks_elapsed_us as i64,
                i64
            ),
            (
                "process_switch_bank_events_elapsed_us",
                self.process_switch_bank_events_elapsed_us as i64,
                i64
            ),
            (
                "wait_receive_elapsed_us",
                self.wait_receive_elapsed_us as i64,
                i64
            ),
            (
                "generate_new_bank_forks_read_lock_us",
                read_lock_us as i64,
                i64
            ),
            (
                "generate_new_bank_forks_get_slots_since_us",
                get_slots_since_us as i64,
                i64
            ),
            ("generate_new_bank_forks_loop_us", forks_loop_us as i64, i64),
            (
                "generate_new_bank_forks_write_lock_us",
                write_lock_us as i64,
                i64
            ),
            (
                "replay_blockstore_us",
                self.replay_blockstore_us as i64,
                i64
            ),
        );
        *self = Self::default();
        self.last_submit = now;
    }
}

pub struct ReplayStage {
    t_replay: JoinHandle<()>,
}

impl ReplayStage {
    pub fn new(
        config: ReplayStageConfig,
        senders: ReplaySenders,
        receivers: ReplayReceivers,
    ) -> Result<Self, String> {
        let ReplayStageConfig {
            vote_account,
            authorized_voter_keypairs,
            exit,
            leader_schedule_cache,
            replay_forks_threads,
            replay_transactions_threads,
            blockstore,
            bank_forks,
            cluster_info,
            snapshot_controller,
            replay_highest_frozen,
            highest_parent_ready,
            replay_arena,
        } = config;

        let ReplaySenders {
            rpc_subscriptions,
            slot_status_notifier,
            transaction_status_sender,
            entry_notification_sender,
            bank_notification_sender,
            cost_update_sender,
            drop_bank_sender,
            block_metadata_notifier,
            votor_event_sender,
            optimistic_parent_sender,
            footer_certs_sender,
            votor_commitment_sender,
        } = senders;

        let ReplayReceivers {
            ledger_signal_receiver,
            update_parent_receiver,
            optimistic_parent_receiver,
            bank_forks_controller_receiver,
            latest_switch_request,
        } = receivers;

        trace!("replay stage");

        // Start the replay stage loop
        let mut my_pubkey = cluster_info.id();

        let highest_frozen_slot = bank_forks
            .read()
            .unwrap()
            .highest_frozen_bank()
            .map_or(0, |hfs| hfs.slot());
        *replay_highest_frozen.highest_frozen_slot.lock().unwrap() = highest_frozen_slot;

        let run_replay = move || {
            if let Some(arena) = replay_arena {
                arena
                    .bind_current_thread_permanently()
                    .expect("failed to bind replay thread to jemalloc arena");
            }
            let _exit = Finalizer::new(exit.clone());

            let mut progress = Self::initialize_progress(&bank_forks);
            let mut replay_timing = ReplayLoopTiming::default();
            let mut last_vat_status_check = Instant::now();
            // AsyncVerificationProgress does a large allocation for its internal channel, so we
            // keep a free list to avoid doing one of those for each slot
            let mut async_verification_freelist = Vec::new();
            let mut pending_switch = None;
            // Thread pool to (maybe) replay multiple threads in parallel
            let replay_mode = if replay_forks_threads.get() == 1 {
                ForkReplayMode::Serial
            } else {
                let pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(replay_forks_threads.get())
                    .thread_name(|i| format!("solReplayFork{i:02}"))
                    .build()
                    .expect("new rayon threadpool");
                ForkReplayMode::Parallel(pool)
            };
            let replay_verification_worker_pool =
                ReplayVerificationWorkerPool::new(replay_transactions_threads.get());

            let process_active_banks_context = ProcessActiveBanksContext {
                bank_forks: bank_forks.clone(),
                blockstore: blockstore.clone(),
                transaction_status_sender: transaction_status_sender.clone(),
                entry_notification_sender: entry_notification_sender.clone(),
                bank_notification_sender: bank_notification_sender.clone(),
                rpc_subscriptions: rpc_subscriptions.clone(),
                slot_status_notifier: slot_status_notifier.clone(),
                cost_update_sender: cost_update_sender.clone(),
                block_metadata_notifier: block_metadata_notifier.clone(),
                votor_event_sender: votor_event_sender.clone(),
                replay_mode,
                replay_verification_worker_pool,
            };
            let process_bank_forks_context = ProcessBankForksContext {
                bank_forks: bank_forks.clone(),
                blockstore: blockstore.clone(),
                snapshot_controller: snapshot_controller.clone(),
                bank_notification_sender: bank_notification_sender.clone(),
                rpc_subscriptions: rpc_subscriptions.clone(),
                drop_bank_sender: drop_bank_sender.clone(),
                leader_schedule_cache: leader_schedule_cache.clone(),
                votor_commitment_sender,
            };

            loop {
                // Stop getting entries if we get exit signal
                if exit.load(Ordering::Relaxed) {
                    break;
                }

                if matches!(
                    Self::process_bank_forks_commands(
                        &bank_forks_controller_receiver,
                        &process_bank_forks_context,
                        &my_pubkey,
                        &mut progress,
                        &mut async_verification_freelist,
                    ),
                    Err(TryRecvError::Disconnected)
                ) {
                    break;
                }

                handle_update_parent_interrupts(
                    &my_pubkey,
                    &blockstore,
                    &bank_forks,
                    &mut progress,
                    &mut async_verification_freelist,
                    &update_parent_receiver,
                    entry_notification_sender.as_ref(),
                );

                let mut generate_new_bank_forks_time =
                    Measure::start("generate_new_bank_forks_time");
                Self::generate_new_bank_forks(
                    NewBankForksContext {
                        blockstore: &blockstore,
                        bank_forks: &bank_forks,
                        leader_schedule_cache: &leader_schedule_cache,
                        rpc_subscriptions: rpc_subscriptions.as_deref(),
                        slot_status_notifier: &slot_status_notifier,
                        my_pubkey: &my_pubkey,
                    },
                    &mut progress,
                    &mut replay_timing,
                );
                generate_new_bank_forks_time.stop();

                let mut replay_active_banks_time = Measure::start("replay_active_banks_time");
                let new_frozen_slots = Self::process_active_banks(
                    cluster_info.my_shred_version(),
                    &process_active_banks_context,
                    &mut progress,
                    &mut async_verification_freelist,
                    &my_pubkey,
                    &mut replay_timing,
                    &footer_certs_sender,
                );
                let did_complete_bank = !new_frozen_slots.is_empty();
                replay_active_banks_time.stop();

                // VAT health check
                Self::maybe_report_vat_health(
                    &mut last_vat_status_check,
                    &authorized_voter_keypairs,
                    &bank_forks,
                    &vote_account,
                );

                if my_pubkey != cluster_info.id() {
                    let my_old_pubkey = my_pubkey;
                    my_pubkey = cluster_info.id();
                    warn!("Identity changed from {my_old_pubkey} to {my_pubkey}");
                }

                process_soft_dead_slots(
                    &my_pubkey,
                    &blockstore,
                    &bank_forks,
                    &rpc_subscriptions,
                    &slot_status_notifier,
                    &mut progress,
                    &mut async_verification_freelist,
                    entry_notification_sender.as_ref(),
                );
                Self::alpenglow_handle_newly_frozen_banks(
                    &new_frozen_slots,
                    &bank_forks,
                    &my_pubkey,
                    &leader_schedule_cache,
                    &optimistic_parent_sender,
                    &optimistic_parent_receiver,
                    &highest_parent_ready,
                    &replay_highest_frozen,
                );
                let mut process_switch_bank_events_time =
                    Measure::start("process_switch_bank_events_time");
                // BCL inserts its bank before publishing the active leader bank.
                let has_active_leader_bank = bank_forks
                    .read()
                    .unwrap()
                    .banks()
                    .values()
                    .any(|bank| !bank.is_frozen() && !bank.should_replay_from_blockstore());
                if !has_active_leader_bank {
                    Self::process_switch_bank_events(
                        &my_pubkey,
                        &latest_switch_request,
                        &mut pending_switch,
                        &blockstore,
                        &bank_forks,
                        &mut progress,
                        &mut async_verification_freelist,
                    )
                    .expect("Blockstore operations must succeed");
                }
                process_switch_bank_events_time.stop();
                replay_timing.process_switch_bank_events_elapsed_us +=
                    process_switch_bank_events_time.as_us();

                let mut wait_receive_time = Measure::start("wait_receive_time");
                if !did_complete_bank {
                    // only wait for the signal if we did not just process a bank; maybe there are more slots available

                    let timer = Duration::from_millis(100);
                    let bank_forks_command_receiver = bank_forks_controller_receiver.receiver();
                    let set_root_signal_receiver =
                        bank_forks_controller_receiver.set_root_signal_receiver();
                    select! {
                        recv(ledger_signal_receiver) -> result => match result {
                            Err(_) => break,
                            Ok(_) => trace!("blockstore signal"),
                        },
                        recv(bank_forks_command_receiver) -> result => match result {
                            Err(_) => break,
                            Ok(command) => {
                                Self::process_bank_forks_command(
                                    command,
                                    &process_bank_forks_context,
                                    &mut progress,
                                    &mut async_verification_freelist,
                                );
                            }
                        },
                        recv(set_root_signal_receiver) -> result => match result {
                            Err(_) => break,
                            Ok(()) => trace!("bank forks set-root signal"),
                        },
                        default(timer) => (),
                    }
                }
                wait_receive_time.stop();

                replay_timing.update(
                    generate_new_bank_forks_time.as_us(),
                    replay_active_banks_time.as_us(),
                    wait_receive_time.as_us(),
                );
            }
        };
        let t_replay = Builder::new()
            .name("solReplayStage".to_string())
            .spawn(run_replay)
            .unwrap();

        Ok(Self { t_replay })
    }

    fn alpenglow_handle_newly_frozen_banks(
        new_frozen_slots: &[Slot],
        bank_forks: &RwLock<BankForks>,
        my_pubkey: &Pubkey,
        leader_schedule_cache: &LeaderScheduleCache,
        optimistic_parent_sender: &Sender<LeaderWindowInfo>,
        optimistic_parent_receiver: &Receiver<LeaderWindowInfo>,
        highest_parent_ready: &RwLock<(Slot, Block)>,
        replay_highest_frozen: &ReplayHighestFrozen,
    ) {
        let flh_candidate_banks = {
            let bank_forks_r = bank_forks.read().unwrap();
            new_frozen_slots
                .iter()
                .filter_map(|slot| bank_forks_r.get(*slot))
                .filter(|bank| bank.feature_set.snapshot().alpenglow_fast_leader_handover)
                .collect_vec()
        };
        let highest_parent_ready_slot = highest_parent_ready.read().unwrap().0;
        for bank in flh_candidate_banks {
            Self::maybe_notify_of_optimistic_parent(
                &bank,
                my_pubkey,
                leader_schedule_cache,
                optimistic_parent_sender,
                optimistic_parent_receiver,
                highest_parent_ready_slot,
            );
        }

        if let Some(highest) = new_frozen_slots.iter().max() {
            let mut l_highest_frozen = replay_highest_frozen.highest_frozen_slot.lock().unwrap();
            if *highest > *l_highest_frozen {
                // Let the block creation loop know about this new frozen slot
                *l_highest_frozen = *highest;
                replay_highest_frozen.freeze_notification.notify_one();
            }
        }
    }

    fn initialize_progress(bank_forks: &RwLock<BankForks>) -> ProgressMap {
        let mut progress = ProgressMap::default();
        let mut frozen_banks = bank_forks
            .read()
            .unwrap()
            .frozen_banks()
            .map(|(_, bank)| bank)
            .collect_vec();
        frozen_banks.sort_by_key(|bank| bank.slot());
        for bank in frozen_banks {
            progress.insert(bank.slot(), ForkProgress::new_from_bank(&bank, 0, 0, None));
        }
        progress
    }

    #[allow(clippy::too_many_arguments)]
    /// Process switch block events from votor
    ///
    /// When receiving a switch request for block b we attempt to switch out the bank in slot(b)
    /// for b if the bank in slot(b) does not match the block id for b.
    ///
    /// When we need to switch a bank b, we first defer until we've repaired the ancestory of b:
    /// - We must have block b and all of its ancestors up to any ancestor we've already replayed
    /// - If no ancestor is replayed and it links back <= root, we can ignore this request
    ///
    /// Then to perform the switch for b and all of its ancestors identified above we:
    /// - Clear the existing bank (if one exists) in the slot from bank forks and progress
    /// - Use `blockstore::switch_block_from_alternate` to atomically switch the shreds fetched
    ///   by informed repair into the original column
    ///
    /// At this point generate_new_bank_forks can replay the fork up to b
    ///
    /// We only care about the latest switch event. When deferring while waiting for repair we store
    /// this in `pending_switch`
    fn process_switch_bank_events(
        my_pubkey: &Pubkey,
        latest_switch_request: &LatestSwitchRequest,
        pending_switch: &mut Option<SwitchBankEvent>,
        blockstore: &Blockstore,
        bank_forks: &RwLock<BankForks>,
        progress: &mut ProgressMap,
        async_verification_freelist: &mut Vec<AsyncVerificationProgress>,
    ) -> Result<(), BlockstoreError> {
        let root = bank_forks.read().unwrap().root();

        if let Some(event) = latest_switch_request
            .take()
            .filter(|event| event.block().slot > root)
        {
            let block = event.block();
            match pending_switch {
                None => {
                    trace!("{my_pubkey}: Setting empty pending_switch to ({block:?})");
                    *pending_switch = Some(event);
                }
                Some(pending_switch_event) => {
                    if event > *pending_switch_event {
                        trace!(
                            "{my_pubkey}: Overwriting previous switch request {:?} with \
                             ({block:?})",
                            pending_switch_event.block(),
                        );
                        *pending_switch_event = event;
                    }
                }
            }
        };

        let Some(event) = *pending_switch else {
            return Ok(());
        };
        let block = event.block();

        if bank_forks.read().unwrap().block_id(block.slot) == Some(block.block_id.to_hash()) {
            // Nothing to switch
            *pending_switch = None;
            return Ok(());
        }

        // Check if we have received the block and all of its ancestors and collect the ones we
        // need to switch out
        let mut ancestor_slot = block.slot;
        let mut ancestor_block_id = block.block_id.to_hash();
        let mut blocks_to_switch = vec![];
        let mut original_dead_slots_to_clear = BTreeSet::new();
        loop {
            if ancestor_slot <= root {
                // This is either (1) an outdated attempt to switch out the
                // root or (2) attempt to switch to a fork that would end up
                // switching out the root, so we should ignore
                *pending_switch = None;
                return Ok(());
            }

            let Some((slot_meta, location)) =
                blockstore.get_slot_meta_for_block_id(ancestor_slot, ancestor_block_id)?
            else {
                trace!(
                    "{my_pubkey}: Waiting for repair, deferring switch to block {ancestor_slot} \
                     {ancestor_block_id}"
                );
                // Still waiting on repair to finish - keep pending request
                return Ok(());
            };

            if location == BlockLocation::Original {
                if blockstore.is_dead(ancestor_slot) {
                    original_dead_slots_to_clear.insert(ancestor_slot);
                }
            } else {
                blocks_to_switch.push((ancestor_slot, location));
            }

            let parent_slot = slot_meta
                .parent_slot
                .expect("Full slots must have a parent");
            let parent_block_id = slot_meta.parent_block_id;

            if bank_forks.read().unwrap().block_id(parent_slot) == Some(parent_block_id)
                // Genesis cannot be duplicate
                || parent_slot == 0
            {
                info!(
                    "{my_pubkey}: Ancestor in slot {parent_slot} found in bank forks, time to \
                     switch"
                );
                // We have this ancestor replayed, time to switch
                break;
            }

            // Check the next ancestor
            ancestor_block_id = parent_block_id;
            ancestor_slot = parent_slot;
        }

        let slots_to_clear = blocks_to_switch
            .iter()
            .map(|(slot, _)| *slot)
            .chain(original_dead_slots_to_clear.iter().copied());

        info!("{my_pubkey}: Clearing banks for switching: {slots_to_clear:?}");
        Self::clear_slots(
            slots_to_clear,
            bank_forks,
            progress,
            async_verification_freelist,
        );

        // Banks are clear, move shreds in blockstore
        for (slot, location) in blocks_to_switch {
            info!("{my_pubkey}: Switching {slot} from {location:?}");

            // Switch the blockstore data atomically, handles slot meta chaining so generate new bank forks can proceed
            blockstore.switch_block_from_alternate(slot, location)?;
            progress.increment_num_bank_switches(slot);
            info!("{my_pubkey}: Switched {slot} from {location:?}");
        }

        for slot in original_dead_slots_to_clear {
            blockstore.remove_dead_slot(slot)?;
        }

        *pending_switch = None;

        Ok(())
    }

    /// Clear the requested slots and their descendants from progress, bank forks, and shared
    /// caches. Requested slots are purged from shared caches even if their banks no longer exist.
    fn clear_slots(
        slots_to_clear: impl IntoIterator<Item = Slot>,
        bank_forks: &RwLock<BankForks>,
        progress: &mut ProgressMap,
        async_verification_freelist: &mut Vec<AsyncVerificationProgress>,
    ) {
        let (slots_to_purge, banks_to_clear) =
            bank_forks.read().unwrap().slots_to_clear(slots_to_clear);
        if slots_to_purge.is_empty() {
            return;
        }

        // Wait for async verify to complete
        for slot in &slots_to_purge {
            if let Some(replay_progress) = progress.remove(slot) {
                let mut w_replay_progress = replay_progress.replay_progress.write().unwrap();
                let _ = w_replay_progress.wait_for_all_verification_results(&mut 0, &mut 0);
                Self::recycle_async_verification(
                    async_verification_freelist,
                    w_replay_progress.take_async_verification(),
                );
            }
        }

        // Wait for any in progress execution
        for bank in banks_to_clear.iter() {
            bank.quiesce_transaction_execution();
            let _ = bank.wait_for_completed_scheduler();
        }
        let bank_slots_to_clear = banks_to_clear
            .iter()
            .map(|bank| bank.slot())
            .collect::<BTreeSet<_>>();

        // Only dump banks that are still present. `slots_to_purge` can also contain slots that
        // were already removed, but whose shared cache entries still need to be cleared before the
        // slots are revived.
        let (root_bank, slot_bank_ids_to_purge, removed_banks) = {
            let mut w_bank_forks = bank_forks.write().unwrap();

            let root_bank = w_bank_forks.root_bank();
            let bank_slots_to_clear = bank_slots_to_clear
                .into_iter()
                .filter(|slot| w_bank_forks.get(*slot).is_some())
                .collect::<BTreeSet<_>>();
            let (slot_bank_ids_to_purge, removed_banks) =
                w_bank_forks.dump_slots(bank_slots_to_clear.iter(), false);
            (root_bank, slot_bank_ids_to_purge, removed_banks)
        };

        // Clear the accounts for these slots so that any ongoing RPC scans fail.
        // These have to be atomically cleared together in the same batch, in order
        // to prevent RPC from seeing inconsistent results in scans.
        if !slot_bank_ids_to_purge.is_empty() {
            root_bank.remove_unrooted_slots(&slot_bank_ids_to_purge);
        }

        // Once the slots above have been purged, now it's safe to remove the banks from
        // BankForks, allowing the Bank::drop() purging to run and not race with the
        // `remove_unrooted_slots()` call.
        drop(banks_to_clear);
        drop(removed_banks);

        // Clear the shared caches even for requested slots whose banks were already removed.
        // Those slots can be revived by an Alpenglow switch, and stale entries from the old block
        // version must not affect replay of the new version.
        for slot in slots_to_purge {
            root_bank.clear_slot_signatures(slot);
            root_bank.prune_program_cache_by_deployment_slot(slot);
        }
    }

    fn recycle_async_verification(
        async_verification_freelist: &mut Vec<AsyncVerificationProgress>,
        async_verification: Option<AsyncVerificationProgress>,
    ) {
        if let Some(async_verification) = async_verification
            && async_verification_freelist.len() < ASYNC_VERIFICATION_FREELIST_CAPACITY
        {
            async_verification_freelist.push(async_verification);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn replay_blockstore_into_bank(
        my_shred_version: u16,
        process_active_banks_context: &ProcessActiveBanksContext,
        bank: &BankWithScheduler,
        replay_stats: &RwLock<ReplaySlotStats>,
        replay_progress: &RwLock<ConfirmationProgress>,
        finalization_cert_sender: &Sender<SmallVec<[Certificate; 2]>>,
    ) -> result::Result<usize, BlockstoreProcessorError> {
        let mut w_replay_stats = replay_stats.write().unwrap();
        let mut w_replay_progress = replay_progress.write().unwrap();
        let tx_count_before = w_replay_progress.num_txs;
        blockstore_processor::confirm_slot(
            &process_active_banks_context.blockstore,
            bank,
            my_shred_version,
            &process_active_banks_context.replay_verification_worker_pool,
            &mut w_replay_stats,
            &mut w_replay_progress,
            false,
            process_active_banks_context
                .entry_notification_sender
                .as_ref(),
            Some(finalization_cert_sender),
            false,
        )?;
        let tx_count_after = w_replay_progress.num_txs;
        let tx_count = tx_count_after - tx_count_before;
        Ok(tx_count)
    }

    #[allow(clippy::too_many_arguments)]
    fn maybe_report_vat_health(
        last_vat_status_check: &mut Instant,
        authorized_voter_keypairs: &Arc<RwLock<Vec<Arc<Keypair>>>>,
        bank_forks: &Arc<RwLock<BankForks>>,
        vote_account: &Pubkey,
    ) {
        if last_vat_status_check.elapsed().as_secs() < VAT_STATUS_CHECK_INTERVAL_SECS {
            return;
        }
        *last_vat_status_check = Instant::now();

        let bank = bank_forks.read().unwrap().root_bank();

        let is_voting_validator = !authorized_voter_keypairs.read().unwrap().is_empty();
        if !is_voting_validator {
            return;
        }

        let epoch = bank.epoch();
        if let Err(vat_failure_reason) = bank.get_vat_health_for_next_epoch(vote_account) {
            warn!(
                "VAT Health Check: Currently you will fail the VAT check at the start of epoch {} \
                 meaning that you will be unable to vote or produce blocks in epoch {}. Reason: {}",
                epoch.saturating_add(1),
                epoch.saturating_add(2),
                vat_failure_reason,
            );
        } else {
            info!(
                "VAT Health Check: Passing local VAT checks. Assuming you are in the top {} of \
                 stake, you will be included in voting and block building in epoch {}",
                MAX_ALPENGLOW_VOTE_ACCOUNTS,
                epoch.saturating_add(2),
            );
        }
    }

    fn prepare_active_bank_for_replay(
        process_active_banks_context: &ProcessActiveBanksContext,
        progress: &mut ProgressMap,
        async_verification_freelist: &mut Vec<AsyncVerificationProgress>,
        replay_result: &mut ReplaySlotFromBlockstore,
    ) -> Option<BankReplayTracker> {
        let bank_slot = replay_result.bank_slot;
        if progress
            .get(&bank_slot)
            .is_some_and(|p| p.dead_reason.is_some())
        {
            // If the fork was marked as dead, don't replay it
            debug!("bank_slot {bank_slot:?} is marked dead");
            replay_result.is_slot_dead = true;
            return None;
        }

        let Some(bank) = process_active_banks_context
            .bank_forks
            .read()
            .unwrap()
            .get_with_scheduler(bank_slot)
        else {
            info!("Abandoning replay of unrooted slot {bank_slot}");
            return None;
        };

        let parent_slot = bank.parent_slot();
        let Some(stats) = progress.get(&parent_slot) else {
            info!("Abandoning replay of unrooted slot {bank_slot}");
            return None;
        };
        let num_blocks_on_fork = stats.num_blocks_on_fork + 1;
        let new_dropped_blocks = bank.slot() - parent_slot - 1;
        let num_dropped_blocks_on_fork = stats.num_dropped_blocks_on_fork + new_dropped_blocks;

        let bank_progress = progress.entry(bank.slot()).or_insert_with(|| {
            ForkProgress::new_from_bank(
                &bank,
                num_blocks_on_fork,
                num_dropped_blocks_on_fork,
                async_verification_freelist.pop(),
            )
        });

        Some(BankReplayTracker {
            bank,
            replay_stats: bank_progress.replay_stats.clone(),
            replay_progress: bank_progress.replay_progress.clone(),
        })
    }

    fn prepare_active_banks_for_replay(
        process_active_banks_context: &ProcessActiveBanksContext,
        progress: &mut ProgressMap,
        async_verification_freelist: &mut Vec<AsyncVerificationProgress>,
    ) -> Vec<BankReplayResultTracker> {
        let active_bank_slots = process_active_banks_context
            .bank_forks
            .read()
            .unwrap()
            .active_bank_slots();
        trace!(
            "{} active bank(s) to replay: {active_bank_slots:?}",
            active_bank_slots.len()
        );
        if active_bank_slots.is_empty() {
            return vec![];
        }

        active_bank_slots
            .iter()
            .map(|bank_slot| {
                let mut replay_result = ReplaySlotFromBlockstore::new(*bank_slot);
                let bank_replay_tracker = Self::prepare_active_bank_for_replay(
                    process_active_banks_context,
                    progress,
                    async_verification_freelist,
                    &mut replay_result,
                );
                BankReplayResultTracker {
                    replay_result,
                    bank_replay_tracker,
                }
            })
            .collect()
    }

    fn replay_active_bank(
        my_shred_version: u16,
        process_active_banks_context: &ProcessActiveBanksContext,
        bank_replay_result_tracker: BankReplayResultTracker,
        finalization_cert_sender: &Sender<SmallVec<[Certificate; 2]>>,
    ) -> (ReplaySlotFromBlockstore, Option<u64>) {
        let BankReplayResultTracker {
            mut replay_result,
            bank_replay_tracker,
        } = bank_replay_result_tracker;
        let Some(bank_replay_tracker) = bank_replay_tracker else {
            return (replay_result, None);
        };

        let BankReplayTracker {
            bank,
            replay_stats,
            replay_progress,
        } = bank_replay_tracker;

        if !bank.should_replay_from_blockstore() {
            return (replay_result, None);
        }

        let mut replay_blockstore_time = Measure::start("replay_blockstore_into_bank");
        let blockstore_result = Self::replay_blockstore_into_bank(
            my_shred_version,
            process_active_banks_context,
            &bank,
            &replay_stats,
            &replay_progress,
            finalization_cert_sender,
        );
        replay_blockstore_time.stop();
        replay_result.replay_result = Some(blockstore_result);

        (replay_result, Some(replay_blockstore_time.as_us()))
    }

    /// Returns whether `slot_leader` is this validator.
    fn leader_is_me(slot_leader: &Pubkey, my_pubkey: &Pubkey) -> bool {
        slot_leader == my_pubkey
    }

    fn replay_active_banks(
        my_shred_version: u16,
        process_active_banks_context: &ProcessActiveBanksContext,
        bank_replay_result_trackers: Vec<BankReplayResultTracker>,
        replay_timing: &mut ReplayLoopTiming,
        finalization_cert_sender: &Sender<SmallVec<[Certificate; 2]>>,
    ) -> Vec<ReplaySlotFromBlockstore> {
        match &process_active_banks_context.replay_mode {
            // Skip the overhead of the threadpool if there is only one bank to play
            ForkReplayMode::Parallel(fork_thread_pool) if bank_replay_result_trackers.len() > 1 => {
                let longest_replay_time_us: AtomicU64 = AtomicU64::new(0);
                // Allow for concurrent replaying of slots from different forks.
                let replay_result_vec: Vec<ReplaySlotFromBlockstore> =
                    fork_thread_pool.install(|| {
                        bank_replay_result_trackers
                            .into_par_iter()
                            .map(|bank_replay_result_tracker| {
                                trace!(
                                    "Replay active bank: slot {}, thread_idx {}",
                                    bank_replay_result_tracker.replay_result.bank_slot,
                                    fork_thread_pool.current_thread_index().unwrap_or_default()
                                );
                                let (replay_result, replay_blockstore_us) =
                                    Self::replay_active_bank(
                                        my_shred_version,
                                        process_active_banks_context,
                                        bank_replay_result_tracker,
                                        finalization_cert_sender,
                                    );
                                if let Some(replay_blockstore_us) = replay_blockstore_us {
                                    longest_replay_time_us
                                        .fetch_max(replay_blockstore_us, Ordering::Relaxed);
                                }
                                replay_result
                            })
                            .collect()
                    });
                // Accumulating time across all slots could inflate this number and make it seem like an
                // overly large amount of time is being spent on blockstore compared to other activities.
                replay_timing.replay_blockstore_us +=
                    longest_replay_time_us.load(Ordering::Relaxed);

                replay_result_vec
            }
            ForkReplayMode::Serial | ForkReplayMode::Parallel(_) => bank_replay_result_trackers
                .into_iter()
                .map(|bank_replay_result_tracker| {
                    trace!(
                        "Replay active bank: slot {}",
                        bank_replay_result_tracker.replay_result.bank_slot
                    );
                    let (replay_result, replay_blockstore_us) = Self::replay_active_bank(
                        my_shred_version,
                        process_active_banks_context,
                        bank_replay_result_tracker,
                        finalization_cert_sender,
                    );
                    if let Some(replay_blockstore_us) = replay_blockstore_us {
                        replay_timing.replay_blockstore_us += replay_blockstore_us;
                    }
                    replay_result
                })
                .collect(),
        }
    }

    fn complete_scheduler_replay(
        bank: &BankWithScheduler,
        replay_stats: &RwLock<ReplaySlotStats>,
    ) -> SchedulerReplayOutcome {
        let Some((result, completed_execute_timings)) = bank.wait_for_completed_scheduler() else {
            return SchedulerReplayOutcome::NoSchedulerReplay;
        };

        let metrics = ExecuteBatchesInternalMetrics::new_with_timings_from_all_threads(
            completed_execute_timings,
        );
        replay_stats
            .write()
            .unwrap()
            .batch_execute
            .accumulate(metrics, true);

        SchedulerReplayOutcome::SchedulerReplayCompleted(
            result.map_err(BlockstoreProcessorError::InvalidTransaction),
        )
    }

    fn complete_replay_verification(
        replay_stats: &RwLock<ReplaySlotStats>,
        replay_progress: &RwLock<ConfirmationProgress>,
        async_verification_freelist: &mut Vec<AsyncVerificationProgress>,
    ) -> Result<(), BlockstoreProcessorError> {
        let mut poh_verify_elapsed = 0;
        let mut tx_verify_elapsed = 0;
        let (verify_result, async_verification) = {
            let mut replay_progress = replay_progress.write().unwrap();
            (
                replay_progress.wait_for_all_verification_results(
                    &mut poh_verify_elapsed,
                    &mut tx_verify_elapsed,
                ),
                replay_progress.take_async_verification(),
            )
        };

        {
            let mut stats = replay_stats.write().unwrap();
            stats.poh_verify_elapsed += poh_verify_elapsed;
            stats.transaction_verify_elapsed += tx_verify_elapsed;
        }

        Self::recycle_async_verification(async_verification_freelist, async_verification);
        verify_result
    }

    fn complete_bank_replay(
        bank: &BankWithScheduler,
        bank_progress: &mut ForkProgress,
        async_verification_freelist: &mut Vec<AsyncVerificationProgress>,
    ) -> Result<CompletedBankReplay, BlockstoreProcessorError> {
        let replay_stats = bank_progress.replay_stats.clone();

        let scheduler_replay_outcome = Self::complete_scheduler_replay(bank, &replay_stats);
        let is_unified_scheduler_enabled = matches!(
            scheduler_replay_outcome,
            SchedulerReplayOutcome::SchedulerReplayCompleted(_)
        );
        let verify_result = Self::complete_replay_verification(
            &replay_stats,
            &bank_progress.replay_progress,
            async_verification_freelist,
        );

        let replay_result = match scheduler_replay_outcome {
            SchedulerReplayOutcome::NoSchedulerReplay => Ok(()),
            SchedulerReplayOutcome::SchedulerReplayCompleted(replay_result) => replay_result,
        };
        replay_result.and(verify_result)?;

        Ok(CompletedBankReplay {
            replay_stats,
            replay_progress: bank_progress.replay_progress.clone(),
            is_unified_scheduler_enabled,
        })
    }

    fn process_replay_results(
        process_active_banks_context: &ProcessActiveBanksContext,
        progress: &mut ProgressMap,
        async_verification_freelist: &mut Vec<AsyncVerificationProgress>,
        replay_result_vec: &[ReplaySlotFromBlockstore],
        my_pubkey: &Pubkey,
    ) -> Vec<Slot> {
        let bank_forks = &process_active_banks_context.bank_forks;

        // TODO: See if processing of blockstore replay results and bank completion can be made thread safe.
        let mut new_frozen_slots = vec![];
        for replay_result in replay_result_vec {
            if replay_result.is_slot_dead {
                continue;
            }

            let bank_slot = replay_result.bank_slot;
            let Some(bank) = &bank_forks.read().unwrap().get_with_scheduler(bank_slot) else {
                info!("Abandoning replay of unrooted slot {bank_slot}");
                continue;
            };
            if let Some(replay_result) = &replay_result.replay_result {
                match replay_result {
                    Ok(_) => {}
                    Err(BlockstoreProcessorError::BlockComponentProcessor(
                        BlockComponentProcessorError::AbandonedBank(update_parent),
                    )) => {
                        handle_abandoned_bank(
                            process_active_banks_context,
                            bank,
                            bank_slot,
                            update_parent,
                            bank_forks,
                            progress,
                            async_verification_freelist,
                        );
                        continue;
                    }
                    Err(err) => {
                        mark_replay_dead_slot(
                            bank,
                            err,
                            progress,
                            &process_active_banks_context.dead_slot_context(),
                        );

                        // don't try to run the below logic to check if the bank is completed
                        continue;
                    }
                }
            }

            assert_eq!(bank_slot, bank.slot());
            if bank.is_complete() {
                let mut bank_complete_time = Measure::start("bank_complete_time");
                let bank_progress = progress
                    .get_mut(&bank.slot())
                    .expect("Bank fork progress entry missing for completed bank");

                let completed_replay = match Self::complete_bank_replay(
                    bank,
                    bank_progress,
                    async_verification_freelist,
                ) {
                    Ok(completed_replay) => completed_replay,
                    Err(err) => {
                        mark_replay_dead_slot(
                            bank,
                            &err,
                            progress,
                            &process_active_banks_context.dead_slot_context(),
                        );
                        // don't try to run the remaining normal processing for the completed bank
                        continue;
                    }
                };
                let is_leader_block = Self::leader_is_me(bank.leader_id(), my_pubkey);

                // The block id is the merkle root of the last data shred
                // For leader blocks, we might not have finished shredding (as it happens asynchronously)
                // so this can return None. If that is the case, broadcast will set the block id once shredding
                // finishes.
                let block_id = process_active_banks_context
                    .blockstore
                    .get_block_id(bank.slot())
                    .expect("Blockstore operations must succeed");
                debug_assert!(block_id.is_some() || !bank.should_replay_from_blockstore());
                if block_id.is_some() {
                    bank.set_block_id(block_id);
                }

                // Freeze the bank before sending to any auxiliary threads that may expect to be
                // operating on a frozen bank.
                // Also if we are not the leader, ensure that our computed hash matches the hash in
                // the block footer.
                let verify_result = if !Self::leader_is_me(bank.leader_id(), my_pubkey) {
                    bank.freeze_and_verify_bank_hash()
                } else {
                    bank.freeze();
                    Ok(())
                };

                datapoint_info!(
                    "bank_frozen",
                    ("slot", bank_slot, i64),
                    ("hash", bank.hash().to_string(), String),
                );

                if let Err((expected_hash, computed_hash)) = verify_result {
                    warn!(
                        "For slot {bank_slot} the leader said the bank hash should be: \
                         {expected_hash} however we computed: {computed_hash}",
                    );

                    datapoint_warn!(
                        "bank_hash_mismatch",
                        ("slot", bank_slot, i64),
                        ("expected", expected_hash.to_string(), String),
                        ("computed", computed_hash.to_string(), String),
                    );

                    mark_replay_dead_slot(
                        bank,
                        &BlockstoreProcessorError::BankHashMismatch(
                            bank_slot,
                            expected_hash,
                            computed_hash,
                        ),
                        progress,
                        &process_active_banks_context.dead_slot_context(),
                    );

                    continue;
                }

                let r_replay_stats = completed_replay.replay_stats.read().unwrap();
                let r_replay_progress = completed_replay.replay_progress.read().unwrap();
                debug!(
                    "bank {} has completed replay from blockstore, contribute to update cost with \
                     {:?}",
                    bank.slot(),
                    r_replay_stats.batch_execute.totals
                );
                new_frozen_slots.push(bank.slot());

                if let Some(transaction_status_sender) = process_active_banks_context
                    .transaction_status_sender
                    .as_ref()
                {
                    transaction_status_sender.send_transaction_status_freeze_message(bank);
                }
                // report cost tracker stats
                process_active_banks_context
                    .cost_update_sender
                    .send(CostUpdate::FrozenBank {
                        bank: bank.clone_without_scheduler(),
                        is_leader_block,
                    })
                    .unwrap_or_else(|err| {
                        warn!("cost_update_sender failed sending bank stats: {err:?}")
                    });

                assert_ne!(bank.hash(), Hash::default());
                bank_progress.bank_hash = Some(bank.hash());
                // For leader banks:
                // 1) Replay finishes before shredding, broadcast_stage will take care of
                //      notifying votor
                // 2) Shredding finishes before replay, we notify here
                //
                // For non leader banks (2) is always true, so notify here
                if bank.block_id().is_some() {
                    // Leader blocks will not have a block id, broadcast stage will
                    // take care of notifying the voting loop
                    let _ =
                        process_active_banks_context
                            .votor_event_sender
                            .send(VotorEvent::Block(CompletedBlock {
                                slot: bank.slot(),
                                bank: bank.clone_without_scheduler(),
                            }));
                }

                if let Some(sender) = process_active_banks_context
                    .bank_notification_sender
                    .as_ref()
                {
                    let dependency_work = sender
                        .dependency_tracker
                        .as_ref()
                        .map(|s| s.get_current_declared_work());
                    sender
                        .sender
                        .send((
                            BankNotification::Frozen(bank.clone_without_scheduler()),
                            dependency_work,
                        ))
                        .unwrap_or_else(|err| warn!("bank_notification_sender failed: {err:?}"));
                }

                if let Some(block_metadata_notifier) = process_active_banks_context
                    .block_metadata_notifier
                    .as_ref()
                {
                    let parent_blockhash = bank
                        .parent()
                        .map(|bank| bank.last_blockhash())
                        .unwrap_or_default();
                    let commission_rate_in_basis_points =
                        bank.feature_set.snapshot().commission_rate_in_basis_points;
                    block_metadata_notifier.notify_block_metadata(
                        bank.parent_slot(),
                        &parent_blockhash.to_string(),
                        bank.slot(),
                        bank.bank_id(),
                        &bank.last_blockhash().to_string(),
                        &bank.get_rewards_and_num_partitions(),
                        Some(bank.clock().unix_timestamp),
                        Some(bank.block_height()),
                        bank.executed_transaction_count(),
                        r_replay_progress.num_entries as u64,
                        commission_rate_in_basis_points,
                    )
                }
                bank_complete_time.stop();

                r_replay_stats.report_stats(
                    bank.slot(),
                    r_replay_progress.num_txs,
                    r_replay_progress.num_entries,
                    r_replay_progress.num_shreds,
                    bank_complete_time.as_us(),
                    completed_replay.is_unified_scheduler_enabled,
                );
            } else {
                trace!(
                    "bank {} not completed tick_height: {}, max_tick_height: {}",
                    bank.slot(),
                    bank.tick_height(),
                    bank.max_tick_height()
                );
            }
        }

        new_frozen_slots
    }

    #[allow(clippy::too_many_arguments)]
    fn process_active_banks(
        my_shred_version: u16,
        process_active_banks_context: &ProcessActiveBanksContext,
        progress: &mut ProgressMap,
        async_verification_freelist: &mut Vec<AsyncVerificationProgress>,
        my_pubkey: &Pubkey,
        replay_timing: &mut ReplayLoopTiming,
        finalization_cert_sender: &Sender<SmallVec<[Certificate; 2]>>,
    ) -> Vec<Slot> /* completed slots */ {
        let bank_replay_result_trackers = Self::prepare_active_banks_for_replay(
            process_active_banks_context,
            progress,
            async_verification_freelist,
        );
        if bank_replay_result_trackers.is_empty() {
            return vec![];
        }

        // Perform replay execution.
        let replay_result_vec = Self::replay_active_banks(
            my_shred_version,
            process_active_banks_context,
            bank_replay_result_trackers,
            replay_timing,
            finalization_cert_sender,
        );

        // Process replay results.
        Self::process_replay_results(
            process_active_banks_context,
            progress,
            async_verification_freelist,
            &replay_result_vec,
            my_pubkey,
        )
    }

    /// Fast leader handover: if we're going to be the next leader, and our leader window
    /// starts on the next slot, then send the bank through the optimistic parent channel.
    fn maybe_notify_of_optimistic_parent(
        bank: &Arc<Bank>,
        my_pubkey: &Pubkey,
        leader_schedule_cache: &LeaderScheduleCache,
        optimistic_parent_sender: &Sender<LeaderWindowInfo>,
        optimistic_parent_receiver: &Receiver<LeaderWindowInfo>,
        highest_parent_ready_slot: Slot,
    ) {
        let next_slot = bank.slot().saturating_add(1);
        if next_slot != first_of_consecutive_leader_slots(next_slot) {
            return;
        }

        // Make sure we're not getting tricked into sending an optimistic parent for a
        // leader window that is too far in the future.
        let parent_window_start = first_of_consecutive_leader_slots(bank.slot());
        if highest_parent_ready_slot < parent_window_start {
            trace!(
                "suppressing optimistic parent {} for window starting at {next_slot}: highest \
                 ParentReady {highest_parent_ready_slot} is before parent window \
                 {parent_window_start}",
                bank.slot()
            );
            return;
        }

        let is_next_leader = leader_schedule_cache
            .slot_leader_at(next_slot, Some(bank))
            .is_some_and(|leader| Self::leader_is_me(&leader.id, my_pubkey));

        if !is_next_leader {
            return;
        }

        // TODO: if the leader has, e.g., two consecutive leader windows, this line here
        // prevents fast leader handover from the first leader window to the second. This is because
        // the block id is only computed after shredding a block, which occurs in broadcast, which
        // happens after replay. This function, on the other hand, is invoked prior to replay.
        //
        // To address this, we should additionally add in an optimistic parent channel that goes
        // from broadcast to replay.
        let Some(block_id) = bank.block_id() else {
            return;
        };

        let end_slot = next_slot.saturating_add(NUM_CONSECUTIVE_LEADER_SLOTS.get() as Slot - 1);
        let leader_window_info = LeaderWindowInfo {
            start_slot: next_slot,
            end_slot,
            parent_block: Block {
                slot: bank.slot(),
                block_id: BlockId::from(block_id),
            },
            block_timer: Instant::now(),
        };

        Self::try_send_latest_optimistic_parent(
            optimistic_parent_sender,
            optimistic_parent_receiver,
            leader_window_info,
        );
    }

    /// Publish an optimistic-parent notification without letting the bounded
    /// channel preserve stale windows.
    ///
    /// If the channel is full, replay drains any queued notifications, keeps the
    /// freshest window by start slot, and attempts to enqueue only that one.
    fn try_send_latest_optimistic_parent(
        optimistic_parent_sender: &Sender<LeaderWindowInfo>,
        optimistic_parent_receiver: &Receiver<LeaderWindowInfo>,
        leader_window_info: LeaderWindowInfo,
    ) {
        let start_slot = leader_window_info.start_slot;
        let end_slot = leader_window_info.end_slot;

        match optimistic_parent_sender.try_send(leader_window_info) {
            Ok(()) => {}
            Err(TrySendError::Full(leader_window_info)) => {
                let mut latest = None;
                let mut queued_count = 0;
                for queued_info in optimistic_parent_receiver.try_iter() {
                    queued_count += 1;
                    latest = Some(match latest {
                        Some(current) => freshest_leader_window(current, queued_info),
                        None => queued_info,
                    });
                }
                let latest = match latest {
                    Some(current) => freshest_leader_window(current, leader_window_info),
                    None => leader_window_info,
                };
                let latest_start_slot = latest.start_slot;
                let latest_end_slot = latest.end_slot;
                let result = optimistic_parent_sender.try_send(latest);
                let dropped_new = usize::from(result.is_err());
                let dropped = queued_count + dropped_new;

                if dropped > 0 {
                    datapoint_info!(
                        "replay_stage-optimistic_parent_notification_dropped",
                        ("start_slot", start_slot, i64),
                        ("end_slot", end_slot, i64),
                        ("latest_start_slot", latest_start_slot, i64),
                        ("latest_end_slot", latest_end_slot, i64),
                        ("dropped_count", dropped, i64),
                    );
                }

                match result {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        trace!(
                            "optimistic_parent_sender remained full after draining {queued_count} \
                             stale notifications for window {start_slot}-{end_slot}"
                        );
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        trace!(
                            "optimistic_parent_sender disconnected while sending window \
                             {start_slot}-{end_slot}"
                        );
                    }
                }
            }
            Err(TrySendError::Disconnected(_)) => {
                trace!(
                    "optimistic_parent_sender disconnected while sending window \
                     {start_slot}-{end_slot}"
                );
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn process_bank_forks_commands(
        bank_forks_controller_receiver: &BankForksCommandReceiver,
        context: &ProcessBankForksContext,
        my_pubkey: &Pubkey,
        progress: &mut ProgressMap,
        async_verification_freelist: &mut Vec<AsyncVerificationProgress>,
    ) -> Result<(), TryRecvError> {
        if let Some(command) = bank_forks_controller_receiver.take_set_root_command() {
            Self::process_set_root_command(command, context, my_pubkey, progress);
        }

        loop {
            let command = bank_forks_controller_receiver.receiver().try_recv()?;
            Self::process_bank_forks_command(
                command,
                context,
                progress,
                async_verification_freelist,
            );
        }
    }

    fn process_set_root_command(
        command: SetRootCommand,
        context: &ProcessBankForksContext,
        my_pubkey: &Pubkey,
        progress: &mut ProgressMap,
    ) {
        if !command.matches_frozen_bank(&context.bank_forks.read().unwrap()) {
            warn!(
                "{my_pubkey}: Ignoring stale SetRoot for slot {} block_id {}; the matching frozen \
                 bank is no longer newer than the applied root",
                command.new_root.slot, command.new_root.block_id,
            );
            return;
        }

        let new_root = command.new_root.slot;
        root_utils::check_and_handle_new_root(
            new_root,
            new_root,
            context.snapshot_controller.as_deref(),
            Some(new_root),
            &context.bank_notification_sender,
            &context.drop_bank_sender,
            &context.blockstore,
            &context.leader_schedule_cache,
            &context.bank_forks,
            context.rpc_subscriptions.as_deref(),
            my_pubkey,
        );
        progress.handle_new_root(&context.bank_forks.read().unwrap());

        // Publish the root only after its root markers are written and bank
        // forks is rooted, so commitment cache readers never see a root the
        // blockstore doesn't have yet.
        commitment::update_commitment_cache(
            my_pubkey,
            CommitmentType::Rooted,
            new_root,
            &context.votor_commitment_sender,
        );
    }

    /// Process a bank forks command
    fn process_bank_forks_command(
        command: BankForksCommand,
        context: &ProcessBankForksContext,
        progress: &mut ProgressMap,
        async_verification_freelist: &mut Vec<AsyncVerificationProgress>,
    ) {
        match command {
            BankForksCommand::InsertBank {
                bank,
                response_sender,
            } => {
                // Check that the bank is still valid to be inserted
                let bank = {
                    let mut bank_forks = context.bank_forks.write().unwrap();
                    if bank_forks.get(bank.slot()).is_some() {
                        // Bank already exists in the bank forks.
                        None
                    } else if bank.parent().is_some_and(|expected_parent| {
                        bank_forks
                            .get(bank.parent_slot())
                            .is_some_and(|current_parent| {
                                current_parent.bank_id() == expected_parent.bank_id()
                            })
                    }) {
                        // The normal case where bank parent meta matches what's in bank forks.
                        Some(bank_forks.insert(*bank))
                    } else {
                        // Purge these banks inline so we are not waiting on the lazy Accounts
                        // Background Service for cleanup.
                        drop(bank_forks);
                        let slot = bank.slot();
                        bank.remove_unrooted_slots(&[(slot, bank.bank_id())]);
                        bank.clear_slot_signatures(slot);
                        bank.prune_program_cache_by_deployment_slot(slot);
                        drop(bank);
                        None
                    }
                };

                response_sender.send(bank).unwrap_or_else(|_| {
                    warn!("bank forks controller insert-bank response receiver dropped")
                });
            }
            BankForksCommand::ClearBank {
                slot,
                response_sender,
            } => {
                Self::clear_slots(
                    [slot],
                    &context.bank_forks,
                    progress,
                    async_verification_freelist,
                );
                response_sender.send(()).unwrap_or_else(|_| {
                    warn!("bank forks controller clear-bank response receiver dropped")
                });
            }
        }
    }

    fn generate_new_bank_forks(
        ctx: NewBankForksContext<'_>,
        progress: &mut ProgressMap,
        replay_timing: &mut ReplayLoopTiming,
    ) {
        let NewBankForksContext {
            blockstore,
            bank_forks,
            leader_schedule_cache,
            rpc_subscriptions,
            slot_status_notifier,
            my_pubkey,
        } = ctx;

        // Find the next slot that chains to the old slot
        let mut generate_new_bank_forks_read_lock =
            Measure::start("generate_new_bank_forks_read_lock");
        let (frozen_banks, frozen_bank_slots, root, known_bank_slots) = {
            let forks = bank_forks.read().unwrap();
            generate_new_bank_forks_read_lock.stop();

            let frozen_banks: HashMap<_, _> = forks.frozen_banks().collect();
            let frozen_bank_slots: Vec<_> = frozen_banks
                .keys()
                .cloned()
                .filter(|slot| {
                    *slot >= forks.root() && progress.get(slot).unwrap().dead_reason.is_none()
                })
                .collect();
            let known_bank_slots = forks.banks().keys().copied().collect::<HashSet<_>>();

            (
                frozen_banks,
                frozen_bank_slots,
                forks.root(),
                known_bank_slots,
            )
        };

        let mut generate_new_bank_forks_get_slots_since =
            Measure::start("generate_new_bank_forks_get_slots_since");
        let next_slots = blockstore
            .get_slots_since(&frozen_bank_slots)
            .expect("Db error");
        generate_new_bank_forks_get_slots_since.stop();

        // Filter out what we've already seen
        trace!("generate new forks {:?}", {
            let mut next_slots = next_slots.iter().collect::<Vec<_>>();
            next_slots.sort();
            next_slots
        });
        let mut generate_new_bank_forks_loop = Measure::start("generate_new_bank_forks_loop");
        let mut new_banks = HashMap::new();
        for (parent_slot, children) in next_slots {
            let parent_bank = frozen_banks
                .get(&parent_slot)
                .expect("missing parent in bank forks");
            for child_slot in children {
                if known_bank_slots.contains(&child_slot) || new_banks.contains_key(&child_slot) {
                    trace!("child already active or frozen {child_slot}");
                    continue;
                }

                debug_assert!(!progress.contains_key(&child_slot));

                let leader = leader_schedule_cache
                    .slot_leader_at(child_slot, Some(parent_bank))
                    .unwrap();

                // We don't create the bank here for live leader banks, as that is handled by BCL
                // and BankingStage. However, a full block may need to be replayed after a
                // SwitchBank event or after repairing a block following a improper set-identity command.
                if Self::leader_is_me(&leader.id, my_pubkey) && !blockstore.is_full(child_slot) {
                    trace!("skipping replay bank creation for own leader slot {child_slot}");
                    continue;
                }

                let replay_offset =
                    match child_bank_replay_start(blockstore, parent_bank, parent_slot, child_slot)
                    {
                        ChildBankReplayStart::FromStart => None,
                        ChildBankReplayStart::FromUpdateParent(num_shreds) => Some(num_shreds),
                        ChildBankReplayStart::Defer => continue,
                    };

                info!("new fork:{child_slot} parent:{parent_slot} root:{root}",);
                let child_bank = Self::new_bank_from_parent_with_notify(
                    parent_bank.clone(),
                    child_slot,
                    root,
                    leader,
                    rpc_subscriptions,
                    slot_status_notifier,
                );
                blockstore_processor::set_alpenglow_ticks(&child_bank);

                new_banks.insert(child_slot, (child_bank, replay_offset));
            }
        }
        generate_new_bank_forks_loop.stop();

        let mut generate_new_bank_forks_write_lock =
            Measure::start("generate_new_bank_forks_write_lock");
        if !new_banks.is_empty() {
            let mut forks = bank_forks.write().unwrap();
            let root = forks.root();
            for (slot, (bank, replay_offset)) in new_banks {
                if slot < root {
                    continue;
                }
                if forks.get(slot).is_some() {
                    continue;
                }
                let Some(parent_bank) = forks.get(bank.parent_slot()) else {
                    continue;
                };
                if let Some(num_shreds) = replay_offset {
                    let fork_progress = ForkProgress::new(parent_bank.last_blockhash(), 0, 0, None);
                    {
                        let mut replay_progress = fork_progress.replay_progress.write().unwrap();
                        replay_progress.num_shreds = num_shreds;
                    }
                    progress.insert(slot, fork_progress);
                }
                forks.insert(bank);
            }
        }
        generate_new_bank_forks_write_lock.stop();
        replay_timing.generate_new_bank_forks_read_lock_us +=
            generate_new_bank_forks_read_lock.as_us();
        replay_timing.generate_new_bank_forks_get_slots_since_us +=
            generate_new_bank_forks_get_slots_since.as_us();
        replay_timing.generate_new_bank_forks_loop_us += generate_new_bank_forks_loop.as_us();
        replay_timing.generate_new_bank_forks_write_lock_us +=
            generate_new_bank_forks_write_lock.as_us();
    }

    pub(crate) fn new_bank_from_parent_with_notify(
        parent: Arc<Bank>,
        slot: u64,
        root_slot: u64,
        leader: SlotLeader,
        rpc_subscriptions: Option<&RpcSubscriptions>,
        slot_status_notifier: &Option<SlotStatusNotifier>,
    ) -> Bank {
        if let Some(rpc_subscriptions) = rpc_subscriptions {
            rpc_subscriptions.notify_slot(slot, parent.slot(), root_slot);
        }
        let parent_slot = parent.slot();
        let bank = Bank::new_from_parent(parent, leader, slot);
        if let Some(slot_status_notifier) = slot_status_notifier {
            slot_status_notifier.read().unwrap().notify_created_bank(
                slot,
                parent_slot,
                bank.bank_id(),
            );
        }
        bank
    }

    pub fn join(self) -> thread::Result<()> {
        self.t_replay.join().map(|_| ())
    }
}
