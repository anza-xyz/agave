//! The `replay_stage` replays transactions broadcast by the leader.
use {
    crate::{
        banking_stage::update_bank_forks_and_poh_recorder_for_new_tpu_bank,
        banking_trace::BankingTracer,
        block_creation_loop::ReplayHighestFrozen,
        cluster_info_vote_listener::{
            DuplicateConfirmedSlotsReceiver, GossipVerifiedVoteHashReceiver, VoteTracker,
        },
        cluster_slots_service::{ClusterSlotsUpdateSender, cluster_slots::ClusterSlots},
        commitment_service::TowerCommitmentAggregationData,
        consensus::{
            BlockhashStatus, ComputedBankState, SWITCH_FORK_THRESHOLD, Stake, SwitchForkDecision,
            Tower, TowerError, VotedStakes,
            fork_choice::{ForkChoice, SelectVoteAndResetForkResult, select_vote_and_reset_forks},
            heaviest_subtree_fork_choice::HeaviestSubtreeForkChoice,
            latest_validator_votes_for_frozen_banks::LatestValidatorVotesForFrozenBanks,
            progress_map::{ForkProgress, ProgressMap, PropagatedStats},
            tower_storage::{SavedTower, SavedTowerVersions, TowerStorage},
            tower_vote_state::TowerVoteState,
        },
        cost_update_service::CostUpdate,
        repair::{
            ancestor_hashes_service::AncestorHashesReplayUpdateSender,
            cluster_slot_state_verifier::*,
            duplicate_repair_status::AncestorDuplicateSlotToRepair,
            repair_service::{
                AncestorDuplicateSlotsReceiver, DumpedSlotsSender, PopularPrunedForksReceiver,
            },
        },
        unfrozen_gossip_verified_vote_hashes::UnfrozenGossipVerifiedVoteHashes,
        voting_service::VoteOp,
        window_service::DuplicateSlotReceiver,
    },
    agave_votor::{
        event::{
            CompletedBlock, LatestSwitchRequest, LeaderWindowInfo, SwitchBankEvent, VotorEvent,
            VotorEventSender,
        },
        root_utils,
        voting_service::BLSOp,
        voting_utils::{self, GenerateVoteTxResult},
    },
    agave_votor_messages::{
        consensus_message::Block,
        migration::{GENESIS_VOTE_REFRESH, MigrationStatus},
        sig_verified_messages::SigVerifiedBatch,
        vote::Vote,
    },
    crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, select},
    itertools::Itertools,
    rayon::{ThreadPool, prelude::*},
    solana_accounts_db::contains::Contains,
    solana_clock::{BankId, Slot},
    solana_geyser_plugin_manager::block_metadata_notifier_interface::BlockMetadataNotifierArc,
    solana_gossip::cluster_info::ClusterInfo,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_leader_schedule::{NUM_CONSECUTIVE_LEADER_SLOTS, SlotLeader},
    solana_ledger::{
        blockstore::{Blockstore, BlockstoreError, UpdateParentReceiver},
        blockstore_meta::BlockLocation,
        blockstore_processor::{
            self, AsyncVerificationProgress, BlockstoreProcessorError, ChainedBlockIdCheck,
            ConfirmationProgress, ExecuteBatchesInternalMetrics, ReplaySlotStats,
            check_chained_block_id,
        },
        entry_notifier_service::EntryNotifierSender,
        leader_schedule_cache::LeaderScheduleCache,
    },
    solana_measure::measure::Measure,
    solana_poh::{
        poh_controller::PohController,
        poh_recorder::{
            GRACE_TICKS_FACTOR, MAX_GRACE_SLOTS, PohLeaderStatus, PohRecorder, SharedLeaderState,
        },
    },
    solana_pubkey::Pubkey,
    solana_rpc::{
        optimistically_confirmed_bank_tracker::{BankNotification, BankNotificationSenderConfig},
        rpc_subscriptions::RpcSubscriptions,
        slot_status_notifier::SlotStatusNotifier,
    },
    solana_runtime::{
        bank::{Bank, MAX_ALPENGLOW_VOTE_ACCOUNTS, NewBankOptions, bank_hash_details},
        bank_forks::BankForks,
        bank_forks_controller::{BankForksCommand, BankForksCommandReceiver, SetRootCommand},
        block_component_processor::BlockComponentProcessorError,
        commitment::BlockCommitmentCache,
        installed_scheduler_pool::BankWithScheduler,
        leader_schedule_utils::first_of_consecutive_leader_slots,
        prioritization_fee_cache::PrioritizationFeeCache,
        snapshot_controller::SnapshotController,
        transaction_execution::TransactionStatusSender,
        vote_sender_types::{ReplayVoteMessage, ReplayVoteSender},
    },
    solana_signer::Signer,
    solana_time_utils::timestamp,
    solana_transaction::Transaction,
    solana_vote::vote_transaction::VoteTransaction,
    solana_vote_program::vote_state::MAX_LOCKOUT_HISTORY,
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
    dead_slots::{
        DeadSlotContext, DeadSlotDuplicateContext, DeadSlotNotifications, mark_replay_dead_slot,
    },
    update_parent::{
        ChildBankReplayStart, child_bank_replay_start, handle_abandoned_bank,
        handle_update_parent_interrupts, process_soft_dead_slots,
    },
};

pub const MAX_ENTRY_RECV_PER_ITER: usize = 512;
pub const SUPERMINORITY_THRESHOLD: f64 = 1f64 / 3f64;
pub const MAX_UNCONFIRMED_SLOTS: usize = 5;
pub const DUPLICATE_LIVENESS_THRESHOLD: f64 = 0.1;
pub const DUPLICATE_THRESHOLD: f64 = 1.0 - SWITCH_FORK_THRESHOLD - DUPLICATE_LIVENESS_THRESHOLD;
const ASYNC_VERIFICATION_FREELIST_CAPACITY: usize = 5;

pub(crate) const MAX_VOTE_SIGNATURES: usize = 200;
const MAX_VOTE_REFRESH_INTERVAL_MILLIS: usize = 5000;
const MAX_REPAIR_RETRY_LOOP_ATTEMPTS: usize = 10;

// Give at least 4 leaders the chance to pack our vote
const REFRESH_VOTE_BLOCKHEIGHT: usize = 16;

const VAT_STATUS_CHECK_INTERVAL_SECS: u64 = 30;

#[derive(PartialEq, Eq, Debug)]
pub enum HeaviestForkFailures {
    LockedOut(u64),
    FailedThreshold(
        Slot,
        /* vote depth */ u64,
        /* Observed stake */ u64,
        /* Total stake */ u64,
    ),
    FailedSwitchThreshold(
        Slot,
        /* Observed stake */ u64,
        /* Total stake */ u64,
    ),
    NoPropagatedConfirmation(
        Slot,
        /* Observed stake */ u64,
        /* Total stake */ u64,
    ),
}

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
    replay_vote_sender: ReplayVoteSender,
    bank_notification_sender: Option<BankNotificationSenderConfig>,
    rpc_subscriptions: Option<Arc<RpcSubscriptions>>,
    slot_status_notifier: Option<SlotStatusNotifier>,
    cluster_slots_update_sender: ClusterSlotsUpdateSender,
    cost_update_sender: Sender<CostUpdate>,
    ancestor_hashes_replay_update_sender: AncestorHashesReplayUpdateSender,
    block_metadata_notifier: Option<BlockMetadataNotifierArc>,
    votor_event_sender: VotorEventSender,
    log_messages_bytes_limit: Option<usize>,
    replay_mode: ForkReplayMode,
    replay_tx_thread_pool: ThreadPool,
    prioritization_fee_cache: Option<Arc<PrioritizationFeeCache>>,
    migration_status: Arc<MigrationStatus>,
}

struct ProcessBankForksContext {
    bank_forks: Arc<RwLock<BankForks>>,
    blockstore: Arc<Blockstore>,
    snapshot_controller: Option<Arc<SnapshotController>>,
    bank_notification_sender: Option<BankNotificationSenderConfig>,
    rpc_subscriptions: Option<Arc<RpcSubscriptions>>,
    drop_bank_sender: Sender<Vec<BankWithScheduler>>,
    leader_schedule_cache: Arc<LeaderScheduleCache>,
}

impl ProcessActiveBanksContext {
    /// Clone the handles needed to publish a dead-slot transition to blockstore,
    /// replay-vote, RPC, and the optional slot-status notifier.
    fn dead_slot_notifications(&self) -> DeadSlotNotifications {
        DeadSlotNotifications {
            blockstore: self.blockstore.clone(),
            rpc_subscriptions: self.rpc_subscriptions.clone(),
            slot_status_notifier: self.slot_status_notifier.clone(),
            replay_vote_sender: self.replay_vote_sender.clone(),
        }
    }

    /// Build the full context needed when a replay error must become a hard or
    /// soft dead-slot transition.
    fn dead_slot_context<'a>(
        &'a self,
        duplicate_slots_to_repair: &'a mut DuplicateSlotsToRepair,
        purge_repair_slot_counter: &'a mut PurgeRepairSlotCounter,
        tbft_structs: Option<&'a mut TowerBFTStructures>,
    ) -> DeadSlotContext<'a> {
        let root = self.bank_forks.read().unwrap().root();

        DeadSlotContext {
            notifications: self.dead_slot_notifications(),
            duplicate: DeadSlotDuplicateContext {
                root,
                duplicate_slots_to_repair,
                ancestor_hashes_replay_update_sender: &self.ancestor_hashes_replay_update_sender,
                purge_repair_slot_counter,
                tbft_structs,
            },
            migration_status: self.migration_status.as_ref(),
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
    /// Alpenglow migration gates that decide replay offsets and vote-only mode.
    migration_status: &'a MigrationStatus,
    /// This validator's identity, used to leave live own-leader banks to BCL.
    my_pubkey: &'a Pubkey,
}

struct LastVoteRefreshTime {
    last_refresh_time: Instant,
    last_print_time: Instant,
}

pub struct TrackedVoteTransaction {
    pub message_hash: Hash,
    pub transaction_blockhash: Hash,
}

#[derive(Default)]
struct SkippedSlotsInfo {
    last_retransmit_slot: u64,
    last_skipped_slot: u64,
}

pub struct TowerBFTStructures {
    pub heaviest_subtree_fork_choice: HeaviestSubtreeForkChoice,
    pub duplicate_slots_tracker: DuplicateSlotsTracker,
    pub duplicate_confirmed_slots: DuplicateConfirmedSlots,
    pub unfrozen_gossip_verified_vote_hashes: UnfrozenGossipVerifiedVoteHashes,
    pub epoch_slots_frozen_slots: EpochSlotsFrozenSlots,
}

struct PartitionInfo {
    partition_start_time: Option<Instant>,
}

impl PartitionInfo {
    fn new() -> Self {
        Self {
            partition_start_time: None,
        }
    }

    fn update(
        &mut self,
        partition_detected: bool,
        heaviest_slot: Slot,
        last_voted_slot: Slot,
        reset_bank_slot: Slot,
        heaviest_fork_failures: Vec<HeaviestForkFailures>,
    ) {
        if self.partition_start_time.is_none() && partition_detected {
            warn!(
                "PARTITION DETECTED waiting to join heaviest fork: {heaviest_slot} last vote: \
                 {last_voted_slot:?}, reset slot: {reset_bank_slot}",
            );
            datapoint_info!(
                "replay_stage-partition-start",
                ("heaviest_slot", heaviest_slot as i64, i64),
                ("last_vote_slot", last_voted_slot as i64, i64),
                ("reset_slot", reset_bank_slot as i64, i64),
                (
                    "heaviest_fork_failure_first",
                    format!("{:?}", heaviest_fork_failures.first()),
                    String
                ),
                (
                    "heaviest_fork_failure_second",
                    format!("{:?}", heaviest_fork_failures.get(1)),
                    String
                ),
            );
            self.partition_start_time = Some(Instant::now());
        } else if self.partition_start_time.is_some() && !partition_detected {
            warn!(
                "PARTITION resolved heaviest fork: {heaviest_slot} last vote: \
                 {last_voted_slot:?}, reset slot: {reset_bank_slot}"
            );
            datapoint_info!(
                "replay_stage-partition-resolved",
                ("heaviest_slot", heaviest_slot as i64, i64),
                ("last_vote_slot", last_voted_slot as i64, i64),
                ("reset_slot", reset_bank_slot as i64, i64),
                (
                    "partition_duration_ms",
                    self.partition_start_time.unwrap().elapsed().as_millis() as i64,
                    i64
                ),
            );
            self.partition_start_time = None;
        }
    }
}

pub struct ReplayStageConfig {
    pub vote_account: Pubkey,
    pub authorized_voter_keypairs: Arc<RwLock<Vec<Arc<Keypair>>>>,
    pub exit: Arc<AtomicBool>,
    pub leader_schedule_cache: Arc<LeaderScheduleCache>,
    pub block_commitment_cache: Arc<RwLock<BlockCommitmentCache>>,
    pub wait_for_vote_to_start_leader: bool,
    pub tower_storage: Arc<dyn TowerStorage>,
    // Stops voting until this slot has been reached. Should be used to avoid
    // duplicate voting which can lead to slashing.
    pub wait_to_vote_slot: Option<Slot>,
    pub replay_forks_threads: NonZeroUsize,
    pub replay_transactions_threads: NonZeroUsize,
    pub blockstore: Arc<Blockstore>,
    pub bank_forks: Arc<RwLock<BankForks>>,
    pub cluster_info: Arc<ClusterInfo>,
    pub poh_recorder: Arc<RwLock<PohRecorder>>,
    pub poh_controller: PohController,
    pub tower: Tower,
    pub vote_tracker: Arc<VoteTracker>,
    pub cluster_slots: Arc<ClusterSlots>,
    pub log_messages_bytes_limit: Option<usize>,
    pub prioritization_fee_cache: Option<Arc<PrioritizationFeeCache>>,
    pub banking_tracer: Arc<BankingTracer>,
    pub snapshot_controller: Option<Arc<SnapshotController>>,
    pub replay_highest_frozen: Arc<ReplayHighestFrozen>,
}

pub struct ReplaySenders {
    pub rpc_subscriptions: Option<Arc<RpcSubscriptions>>,
    pub slot_status_notifier: Option<SlotStatusNotifier>,
    pub transaction_status_sender: Option<TransactionStatusSender>,
    pub entry_notification_sender: Option<EntryNotifierSender>,
    pub bank_notification_sender: Option<BankNotificationSenderConfig>,
    pub ancestor_hashes_replay_update_sender: AncestorHashesReplayUpdateSender,
    pub retransmit_slots_sender: Sender<u64>,
    pub replay_vote_sender: ReplayVoteSender,
    pub cluster_slots_update_sender: Sender<Vec<u64>>,
    pub cost_update_sender: Sender<CostUpdate>,
    pub voting_sender: Sender<VoteOp>,
    pub bls_sender: Sender<BLSOp>,
    pub drop_bank_sender: Sender<Vec<BankWithScheduler>>,
    pub block_metadata_notifier: Option<BlockMetadataNotifierArc>,
    pub dumped_slots_sender: Sender<Vec<(u64, Hash)>>,
    pub votor_event_sender: VotorEventSender,
    pub own_message_sender: Sender<SigVerifiedBatch>,
    pub optimistic_parent_sender: Sender<LeaderWindowInfo>,
    pub lockouts_sender: Sender<TowerCommitmentAggregationData>,
}

pub struct ReplayReceivers {
    pub ledger_signal_receiver: Receiver<bool>,
    pub update_parent_receiver: UpdateParentReceiver,
    pub optimistic_parent_receiver: Receiver<LeaderWindowInfo>,
    pub duplicate_slots_receiver: Receiver<u64>,
    pub ancestor_duplicate_slots_receiver: Receiver<AncestorDuplicateSlotToRepair>,
    pub duplicate_confirmed_slots_receiver: Receiver<Vec<(u64, Hash)>>,
    pub gossip_verified_vote_hash_receiver: Receiver<(Pubkey, u64, Hash)>,
    pub popular_pruned_forks_receiver: Receiver<Vec<u64>>,
    pub bank_forks_controller_receiver: BankForksCommandReceiver,
    pub latest_switch_request: LatestSwitchRequest,
}

/// Timing information for the ReplayStage main processing loop
#[derive(Default)]
struct ReplayLoopTiming {
    last_submit: u64,
    loop_count: u64,
    collect_frozen_banks_elapsed_us: u64,
    compute_bank_stats_elapsed_us: u64,
    select_vote_and_reset_forks_elapsed_us: u64,
    start_leader_elapsed_us: u64,
    reset_bank_elapsed_us: u64,
    voting_elapsed_us: u64,
    generate_vote_us: u64,
    update_commitment_cache_us: u64,
    select_forks_elapsed_us: u64,
    compute_slot_stats_elapsed_us: u64,
    generate_new_bank_forks_elapsed_us: u64,
    replay_active_banks_elapsed_us: u64,
    wait_receive_elapsed_us: u64,
    heaviest_fork_failures_elapsed_us: u64,
    bank_count: u64,
    process_ancestor_hashes_duplicate_slots_elapsed_us: u64,
    process_duplicate_confirmed_slots_elapsed_us: u64,
    process_duplicate_slots_elapsed_us: u64,
    process_unfrozen_gossip_verified_vote_hashes_elapsed_us: u64,
    process_popular_pruned_forks_elapsed_us: u64,
    process_switch_bank_events_elapsed_us: u64,
    repair_correct_slots_elapsed_us: u64,
    retransmit_not_propagated_elapsed_us: u64,
    generate_new_bank_forks_read_lock_us: Saturating<u64>,
    generate_new_bank_forks_get_slots_since_us: Saturating<u64>,
    generate_new_bank_forks_loop_us: Saturating<u64>,
    generate_new_bank_forks_write_lock_us: Saturating<u64>,
    // When processing multiple forks concurrently, only captures the longest fork
    replay_blockstore_us: u64,
}
impl ReplayLoopTiming {
    #[allow(clippy::too_many_arguments)]
    fn update_non_alpenglow(
        &mut self,
        collect_frozen_banks_elapsed_us: u64,
        compute_bank_stats_elapsed_us: u64,
        select_vote_and_reset_forks_elapsed_us: u64,
        reset_bank_elapsed_us: u64,
        voting_elapsed_us: u64,
        select_forks_elapsed_us: u64,
        compute_slot_stats_elapsed_us: u64,
        heaviest_fork_failures_elapsed_us: u64,
        bank_count: u64,
        process_ancestor_hashes_duplicate_slots_elapsed_us: u64,
        process_duplicate_confirmed_slots_elapsed_us: u64,
        process_unfrozen_gossip_verified_vote_hashes_elapsed_us: u64,
        process_popular_pruned_forks_elapsed_us: u64,
        process_duplicate_slots_elapsed_us: u64,
        repair_correct_slots_elapsed_us: u64,
        retransmit_not_propagated_elapsed_us: u64,
        start_leader_elapsed_us: u64,
    ) {
        self.collect_frozen_banks_elapsed_us += collect_frozen_banks_elapsed_us;
        self.compute_bank_stats_elapsed_us += compute_bank_stats_elapsed_us;
        self.select_vote_and_reset_forks_elapsed_us += select_vote_and_reset_forks_elapsed_us;
        self.reset_bank_elapsed_us += reset_bank_elapsed_us;
        self.voting_elapsed_us += voting_elapsed_us;
        self.select_forks_elapsed_us += select_forks_elapsed_us;
        self.compute_slot_stats_elapsed_us += compute_slot_stats_elapsed_us;
        self.heaviest_fork_failures_elapsed_us += heaviest_fork_failures_elapsed_us;
        self.bank_count += bank_count;
        self.process_ancestor_hashes_duplicate_slots_elapsed_us +=
            process_ancestor_hashes_duplicate_slots_elapsed_us;
        self.process_duplicate_confirmed_slots_elapsed_us +=
            process_duplicate_confirmed_slots_elapsed_us;
        self.process_unfrozen_gossip_verified_vote_hashes_elapsed_us +=
            process_unfrozen_gossip_verified_vote_hashes_elapsed_us;
        self.process_popular_pruned_forks_elapsed_us += process_popular_pruned_forks_elapsed_us;
        self.process_duplicate_slots_elapsed_us += process_duplicate_slots_elapsed_us;
        self.repair_correct_slots_elapsed_us += repair_correct_slots_elapsed_us;
        self.retransmit_not_propagated_elapsed_us += retransmit_not_propagated_elapsed_us;
        self.start_leader_elapsed_us += start_leader_elapsed_us;
    }

    fn update_common(
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

        if elapsed_ms > 1000 {
            datapoint_info!(
                "replay-loop-voting-stats",
                ("generate_vote_us", self.generate_vote_us, i64),
                (
                    "update_commitment_cache_us",
                    self.update_commitment_cache_us,
                    i64
                ),
            );
            let &mut ReplayLoopTiming {
                generate_new_bank_forks_read_lock_us:
                    Saturating(generate_new_bank_forks_read_lock_us),
                generate_new_bank_forks_get_slots_since_us:
                    Saturating(generate_new_bank_forks_get_slots_since_us),
                generate_new_bank_forks_loop_us: Saturating(generate_new_bank_forks_loop_us),
                generate_new_bank_forks_write_lock_us:
                    Saturating(generate_new_bank_forks_write_lock_us),
                ..
            } = self;
            datapoint_info!(
                "replay-loop-timing-stats",
                ("loop_count", self.loop_count as i64, i64),
                ("total_elapsed_us", elapsed_ms * 1000, i64),
                (
                    "collect_frozen_banks_elapsed_us",
                    self.collect_frozen_banks_elapsed_us as i64,
                    i64
                ),
                (
                    "compute_bank_stats_elapsed_us",
                    self.compute_bank_stats_elapsed_us as i64,
                    i64
                ),
                (
                    "select_vote_and_reset_forks_elapsed_us",
                    self.select_vote_and_reset_forks_elapsed_us as i64,
                    i64
                ),
                (
                    "start_leader_elapsed_us",
                    self.start_leader_elapsed_us as i64,
                    i64
                ),
                (
                    "reset_bank_elapsed_us",
                    self.reset_bank_elapsed_us as i64,
                    i64
                ),
                ("voting_elapsed_us", self.voting_elapsed_us as i64, i64),
                (
                    "select_forks_elapsed_us",
                    self.select_forks_elapsed_us as i64,
                    i64
                ),
                (
                    "compute_slot_stats_elapsed_us",
                    self.compute_slot_stats_elapsed_us as i64,
                    i64
                ),
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
                    "process_ancestor_hashes_duplicate_slots_elapsed_us",
                    self.process_ancestor_hashes_duplicate_slots_elapsed_us as i64,
                    i64
                ),
                (
                    "process_duplicate_confirmed_slots_elapsed_us",
                    self.process_duplicate_confirmed_slots_elapsed_us as i64,
                    i64
                ),
                (
                    "process_unfrozen_gossip_verified_vote_hashes_elapsed_us",
                    self.process_unfrozen_gossip_verified_vote_hashes_elapsed_us as i64,
                    i64
                ),
                (
                    "process_popular_pruned_forks_elapsed_us",
                    self.process_popular_pruned_forks_elapsed_us as i64,
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
                    "heaviest_fork_failures_elapsed_us",
                    self.heaviest_fork_failures_elapsed_us as i64,
                    i64
                ),
                ("bank_count", self.bank_count as i64, i64),
                (
                    "process_duplicate_slots_elapsed_us",
                    self.process_duplicate_slots_elapsed_us as i64,
                    i64
                ),
                (
                    "repair_correct_slots_elapsed_us",
                    self.repair_correct_slots_elapsed_us as i64,
                    i64
                ),
                (
                    "retransmit_not_propagated_elapsed_us",
                    self.retransmit_not_propagated_elapsed_us as i64,
                    i64
                ),
                (
                    "generate_new_bank_forks_read_lock_us",
                    generate_new_bank_forks_read_lock_us as i64,
                    i64
                ),
                (
                    "generate_new_bank_forks_get_slots_since_us",
                    generate_new_bank_forks_get_slots_since_us as i64,
                    i64
                ),
                (
                    "generate_new_bank_forks_loop_us",
                    generate_new_bank_forks_loop_us as i64,
                    i64
                ),
                (
                    "generate_new_bank_forks_write_lock_us",
                    generate_new_bank_forks_write_lock_us as i64,
                    i64
                ),
                (
                    "replay_blockstore_us",
                    self.replay_blockstore_us as i64,
                    i64
                ),
            );
            *self = ReplayLoopTiming::default();
            self.last_submit = now;
        }
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
            block_commitment_cache,
            wait_for_vote_to_start_leader,
            tower_storage,
            wait_to_vote_slot,
            replay_forks_threads,
            replay_transactions_threads,
            blockstore,
            bank_forks,
            cluster_info,
            poh_recorder,
            mut poh_controller,
            mut tower,
            vote_tracker,
            cluster_slots,
            log_messages_bytes_limit,
            prioritization_fee_cache,
            banking_tracer,
            snapshot_controller,
            replay_highest_frozen,
        } = config;

        let ReplaySenders {
            rpc_subscriptions,
            slot_status_notifier,
            transaction_status_sender,
            entry_notification_sender,
            bank_notification_sender,
            ancestor_hashes_replay_update_sender,
            retransmit_slots_sender,
            replay_vote_sender,
            cluster_slots_update_sender,
            cost_update_sender,
            voting_sender,
            bls_sender,
            drop_bank_sender,
            block_metadata_notifier,
            dumped_slots_sender,
            votor_event_sender,
            own_message_sender,
            optimistic_parent_sender,
            lockouts_sender,
        } = senders;

        let ReplayReceivers {
            ledger_signal_receiver,
            update_parent_receiver,
            optimistic_parent_receiver,
            duplicate_slots_receiver,
            ancestor_duplicate_slots_receiver,
            duplicate_confirmed_slots_receiver,
            gossip_verified_vote_hash_receiver,
            popular_pruned_forks_receiver,
            bank_forks_controller_receiver,
            latest_switch_request,
        } = receivers;

        trace!("replay stage");

        // Start the replay stage loop
        let migration_status = bank_forks.read().unwrap().migration_status();
        let mut identity_keypair = cluster_info.keypair().clone();
        let mut my_pubkey = identity_keypair.pubkey();

        let mut highest_frozen_slot = bank_forks
            .read()
            .unwrap()
            .highest_frozen_bank()
            .map_or(0, |hfs| hfs.slot());
        *replay_highest_frozen.highest_frozen_slot.lock().unwrap() = highest_frozen_slot;

        let run_replay = move || {
            let _exit = Finalizer::new(exit.clone());

            if my_pubkey != tower.node_pubkey {
                // set-identity was called during the startup procedure, ensure the tower is consistent
                // before starting the loop. further calls to set-identity will reload the tower in the loop
                let my_old_pubkey = tower.node_pubkey;
                if !migration_status.is_alpenglow_enabled() {
                    tower = match Self::load_tower(
                        tower_storage.as_ref(),
                        &my_pubkey,
                        &vote_account,
                        &bank_forks,
                    ) {
                        Ok(tower) => tower,
                        Err(err) => {
                            error!(
                                "Unable to load new tower when attempting to change identity from \
                                 {my_old_pubkey} to {my_pubkey} on ReplayStage startup, Exiting: \
                                 {err}"
                            );
                            // drop(_exit) will set the exit flag, eventually tearing down the entire process
                            return;
                        }
                    };
                    warn!("Identity changed during startup from {my_old_pubkey} to {my_pubkey}");
                }
                migration_status.set_pubkey(my_pubkey);
            }
            let (mut progress, heaviest_subtree_fork_choice) =
                Self::initialize_progress_and_fork_choice_with_locked_bank_forks(
                    &bank_forks,
                    &my_pubkey,
                    &vote_account,
                    &blockstore,
                );
            let mut current_leader = None;
            let mut last_reset = Hash::default();
            let mut last_reset_bank_descendants = Vec::new();
            let mut partition_info = PartitionInfo::new();
            let mut skipped_slots_info = SkippedSlotsInfo::default();
            let mut replay_timing = ReplayLoopTiming::default();
            let duplicate_slots_tracker = DuplicateSlotsTracker::default();
            let duplicate_confirmed_slots = DuplicateConfirmedSlots::default();
            let epoch_slots_frozen_slots = EpochSlotsFrozenSlots::default();
            let mut duplicate_slots_to_repair = DuplicateSlotsToRepair::default();
            let mut purge_repair_slot_counter = PurgeRepairSlotCounter::default();
            let unfrozen_gossip_verified_vote_hashes = UnfrozenGossipVerifiedVoteHashes::default();
            let mut latest_validator_votes_for_frozen_banks =
                LatestValidatorVotesForFrozenBanks::default();
            let mut vote_slots = HashSet::default();
            let mut tracked_vote_transactions: Vec<TrackedVoteTransaction> = Vec::new();
            let mut has_new_vote_been_rooted = !wait_for_vote_to_start_leader;
            let mut last_vote_refresh_time = LastVoteRefreshTime {
                last_refresh_time: Instant::now(),
                last_print_time: Instant::now(),
            };
            let mut last_genesis_vote_refresh_time = Instant::now();
            let mut last_vat_status_check = Instant::now();
            let mut tbft_structs = TowerBFTStructures {
                heaviest_subtree_fork_choice,
                duplicate_slots_tracker,
                duplicate_confirmed_slots,
                unfrozen_gossip_verified_vote_hashes,
                epoch_slots_frozen_slots,
            };
            // AsyncVerificationProgress does a large allocation for its internal channel, so we
            // keep a free list to avoid doing one of those for each slot
            let mut async_verification_freelist = Vec::new();
            let mut pending_switch = None;
            let working_bank = {
                let r_bank_forks = bank_forks.read().unwrap();
                r_bank_forks.working_bank()
            };
            let mut last_threshold_failure_slot = 0;
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
            // Thread pool to replay multiple transactions within one block in parallel
            let replay_tx_thread_pool = rayon::ThreadPoolBuilder::new()
                .num_threads(replay_transactions_threads.get())
                .thread_name(|i| format!("solReplayTx{i:02}"))
                .build()
                .expect("new rayon threadpool");

            let process_active_banks_context = ProcessActiveBanksContext {
                bank_forks: bank_forks.clone(),
                blockstore: blockstore.clone(),
                transaction_status_sender: transaction_status_sender.clone(),
                entry_notification_sender: entry_notification_sender.clone(),
                replay_vote_sender: replay_vote_sender.clone(),
                bank_notification_sender: bank_notification_sender.clone(),
                rpc_subscriptions: rpc_subscriptions.clone(),
                slot_status_notifier: slot_status_notifier.clone(),
                cluster_slots_update_sender: cluster_slots_update_sender.clone(),
                cost_update_sender: cost_update_sender.clone(),
                ancestor_hashes_replay_update_sender: ancestor_hashes_replay_update_sender.clone(),
                block_metadata_notifier: block_metadata_notifier.clone(),
                votor_event_sender: votor_event_sender.clone(),
                log_messages_bytes_limit,
                replay_mode,
                replay_tx_thread_pool,
                prioritization_fee_cache: prioritization_fee_cache.clone(),
                migration_status: migration_status.clone(),
            };
            let process_bank_forks_context = ProcessBankForksContext {
                bank_forks: bank_forks.clone(),
                blockstore: blockstore.clone(),
                snapshot_controller: snapshot_controller.clone(),
                bank_notification_sender: bank_notification_sender.clone(),
                rpc_subscriptions: rpc_subscriptions.clone(),
                drop_bank_sender: drop_bank_sender.clone(),
                leader_schedule_cache: leader_schedule_cache.clone(),
            };

            let poh_shared_leader_state = poh_recorder.read().unwrap().shared_leader_state();
            if !migration_status.is_alpenglow_enabled() {
                // This reset is handled in block creation loop for alpenglow
                Self::reset_poh_recorder(
                    &my_pubkey,
                    &blockstore,
                    working_bank,
                    &mut poh_controller,
                    &leader_schedule_cache,
                );
                // initially we wait for poh service to pick up the bank.
                while poh_controller.has_pending_message() && !exit.load(Ordering::Relaxed) {}
            }

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
                    &replay_vote_sender,
                    migration_status.as_ref(),
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
                        migration_status: migration_status.as_ref(),
                        my_pubkey: &my_pubkey,
                    },
                    &mut progress,
                    &mut replay_timing,
                );
                generate_new_bank_forks_time.stop();

                // We either have a bank currently, OR there is a pending message to either reset or set
                // the bank.
                let tpu_has_bank = poh_shared_leader_state.load().working_bank().is_some()
                    || poh_controller.has_pending_message();

                let mut replay_active_banks_time = Measure::start("replay_active_banks_time");
                let (mut ancestors, mut descendants) = {
                    let r_bank_forks = bank_forks.read().unwrap();
                    (r_bank_forks.ancestors(), r_bank_forks.descendants())
                };
                let new_frozen_slots = Self::process_active_banks(
                    cluster_info.my_shred_version(),
                    &process_active_banks_context,
                    &mut progress,
                    &mut async_verification_freelist,
                    &mut latest_validator_votes_for_frozen_banks,
                    &mut duplicate_slots_to_repair,
                    &mut purge_repair_slot_counter,
                    (!migration_status.is_alpenglow_enabled()).then_some(&mut tbft_structs),
                    &my_pubkey,
                    &vote_account,
                    &mut replay_timing,
                    &own_message_sender,
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

                // Check if we've completed the migration conditions
                if migration_status.is_ready_to_enable() {
                    Self::enable_alpenglow(
                        &exit,
                        &my_pubkey,
                        migration_status.as_ref(),
                        bank_forks.as_ref(),
                        blockstore.as_ref(),
                        &mut poh_controller,
                        &poh_shared_leader_state,
                        leader_schedule_cache.as_ref(),
                        &mut ancestors,
                        &mut descendants,
                        &mut progress,
                    );
                }

                if migration_status.is_alpenglow_enabled() {
                    if my_pubkey != cluster_info.id() {
                        identity_keypair = cluster_info.keypair();
                        let my_old_pubkey = my_pubkey;
                        my_pubkey = identity_keypair.pubkey();
                        migration_status.set_pubkey(my_pubkey);

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
                        &replay_vote_sender,
                        migration_status.as_ref(),
                    );
                    Self::alpenglow_handle_newly_frozen_banks(
                        &new_frozen_slots,
                        &migration_status,
                        &bank_forks,
                        &my_pubkey,
                        &leader_schedule_cache,
                        &optimistic_parent_sender,
                        &optimistic_parent_receiver,
                        &replay_highest_frozen,
                        &mut highest_frozen_slot,
                    );
                    let mut process_switch_bank_events_time =
                        Measure::start("process_switch_bank_events_time");
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
                    process_switch_bank_events_time.stop();
                    replay_timing.process_switch_bank_events_elapsed_us +=
                        process_switch_bank_events_time.as_us();

                    // Banks might have been switched above, these maps are no longer accurate
                    drop(ancestors);
                    drop(descendants);
                } else {
                    let forks_root = bank_forks.read().unwrap().root();
                    // Process cluster-agreed versions of duplicate slots for which we potentially
                    // have the wrong version. Our version was dead or pruned.
                    // Signalled by ancestor_hashes_service.
                    let mut process_ancestor_hashes_duplicate_slots_time =
                        Measure::start("process_ancestor_hashes_duplicate_slots");
                    Self::process_ancestor_hashes_duplicate_slots(
                        &my_pubkey,
                        &blockstore,
                        &ancestor_duplicate_slots_receiver,
                        &mut tbft_structs.duplicate_slots_tracker,
                        &tbft_structs.duplicate_confirmed_slots,
                        &mut tbft_structs.epoch_slots_frozen_slots,
                        &progress,
                        &mut tbft_structs.heaviest_subtree_fork_choice,
                        &bank_forks,
                        &mut duplicate_slots_to_repair,
                        &ancestor_hashes_replay_update_sender,
                        &mut purge_repair_slot_counter,
                    );
                    process_ancestor_hashes_duplicate_slots_time.stop();

                    // Check for any newly duplicate confirmed slots detected from gossip / replay
                    // Note: since this is tracked using both gossip & replay votes, stake is not
                    // rolled up from descendants.
                    let mut process_duplicate_confirmed_slots_time =
                        Measure::start("process_duplicate_confirmed_slots");
                    Self::process_duplicate_confirmed_slots(
                        &duplicate_confirmed_slots_receiver,
                        &blockstore,
                        &mut tbft_structs.duplicate_slots_tracker,
                        &mut tbft_structs.duplicate_confirmed_slots,
                        &mut tbft_structs.epoch_slots_frozen_slots,
                        &bank_forks,
                        &progress,
                        &mut tbft_structs.heaviest_subtree_fork_choice,
                        &mut duplicate_slots_to_repair,
                        &ancestor_hashes_replay_update_sender,
                        &mut purge_repair_slot_counter,
                    );
                    process_duplicate_confirmed_slots_time.stop();

                    // Ingest any new verified votes from gossip. Important for fork choice
                    // and switching proofs because these may be votes that haven't yet been
                    // included in a block, so we may not have yet observed these votes just
                    // by replaying blocks.
                    let mut process_unfrozen_gossip_verified_vote_hashes_time =
                        Measure::start("process_gossip_verified_vote_hashes");
                    Self::process_gossip_verified_vote_hashes(
                        &gossip_verified_vote_hash_receiver,
                        &mut tbft_structs.unfrozen_gossip_verified_vote_hashes,
                        &tbft_structs.heaviest_subtree_fork_choice,
                        &mut latest_validator_votes_for_frozen_banks,
                    );
                    for _ in gossip_verified_vote_hash_receiver.try_iter() {}
                    process_unfrozen_gossip_verified_vote_hashes_time.stop();

                    let mut process_popular_pruned_forks_time =
                        Measure::start("process_popular_pruned_forks_time");
                    // Check for "popular" (52+% stake aggregated across versions/descendants) forks
                    // that are pruned, which would not be detected by normal means.
                    // Signalled by `repair_service`.
                    Self::process_popular_pruned_forks(
                        &popular_pruned_forks_receiver,
                        &blockstore,
                        &mut tbft_structs.duplicate_slots_tracker,
                        &mut tbft_structs.epoch_slots_frozen_slots,
                        &bank_forks,
                        &mut tbft_structs.heaviest_subtree_fork_choice,
                        &mut duplicate_slots_to_repair,
                        &ancestor_hashes_replay_update_sender,
                        &mut purge_repair_slot_counter,
                    );
                    process_popular_pruned_forks_time.stop();

                    // Check to remove any duplicated slots from fork choice
                    let mut process_duplicate_slots_time =
                        Measure::start("process_duplicate_slots");
                    if !tpu_has_bank {
                        Self::process_duplicate_slots(
                            &blockstore,
                            &duplicate_slots_receiver,
                            &mut tbft_structs.duplicate_slots_tracker,
                            &tbft_structs.duplicate_confirmed_slots,
                            &mut tbft_structs.epoch_slots_frozen_slots,
                            &bank_forks,
                            &progress,
                            &mut tbft_structs.heaviest_subtree_fork_choice,
                            &mut duplicate_slots_to_repair,
                            &ancestor_hashes_replay_update_sender,
                            &mut purge_repair_slot_counter,
                        );
                    }
                    process_duplicate_slots_time.stop();

                    let mut collect_frozen_banks_time = Measure::start("frozen_banks");
                    let mut frozen_banks: Vec<_> = bank_forks
                        .read()
                        .unwrap()
                        .frozen_banks()
                        .filter(|(slot, _bank)| *slot >= forks_root)
                        .map(|(_slot, bank)| bank)
                        .collect();
                    collect_frozen_banks_time.stop();

                    let mut compute_bank_stats_time = Measure::start("compute_bank_stats");
                    let newly_computed_slot_stats = Self::compute_bank_stats(
                        &vote_account,
                        &ancestors,
                        &mut frozen_banks,
                        &mut tower,
                        &mut progress,
                        &vote_tracker,
                        &cluster_slots,
                        &bank_forks,
                        &mut tbft_structs.heaviest_subtree_fork_choice,
                        &mut latest_validator_votes_for_frozen_banks,
                        &mut vote_slots,
                        migration_status.as_ref(),
                    );
                    compute_bank_stats_time.stop();

                    // Check if we should vote / refresh our genesis vote
                    if last_genesis_vote_refresh_time.elapsed() > GENESIS_VOTE_REFRESH
                        && migration_status.is_in_migration()
                        && Self::maybe_send_genesis_vote(
                            cluster_info.my_shred_version(),
                            migration_status.as_ref(),
                            bank_forks.as_ref(),
                            vote_account,
                            &identity_keypair,
                            &authorized_voter_keypairs,
                            &own_message_sender,
                            &bls_sender,
                        )
                    {
                        last_genesis_vote_refresh_time = Instant::now();
                    }

                    let mut compute_slot_stats_time = Measure::start("compute_slot_stats_time");
                    for slot in newly_computed_slot_stats {
                        let fork_stats = progress.get_fork_stats(slot).unwrap();
                        let duplicate_confirmed_forks = Self::tower_duplicate_confirmed_forks(
                            &tower,
                            &fork_stats.voted_stakes,
                            fork_stats.total_stake,
                            &progress,
                            &bank_forks,
                        );

                        Self::mark_slots_duplicate_confirmed(
                            &duplicate_confirmed_forks,
                            &blockstore,
                            &bank_forks,
                            &mut progress,
                            &mut tbft_structs.duplicate_slots_tracker,
                            &mut tbft_structs.heaviest_subtree_fork_choice,
                            &mut tbft_structs.epoch_slots_frozen_slots,
                            &mut duplicate_slots_to_repair,
                            &ancestor_hashes_replay_update_sender,
                            &mut purge_repair_slot_counter,
                            &mut tbft_structs.duplicate_confirmed_slots,
                        );
                    }
                    compute_slot_stats_time.stop();

                    let mut select_forks_time = Measure::start("select_forks_time");
                    let (heaviest_bank, heaviest_bank_on_same_voted_fork) = tbft_structs
                        .heaviest_subtree_fork_choice
                        .select_forks(&frozen_banks, &tower, &progress, &ancestors, &bank_forks);
                    select_forks_time.stop();

                    let mut select_vote_and_reset_forks_time =
                        Measure::start("select_vote_and_reset_forks");
                    let SelectVoteAndResetForkResult {
                        vote_bank,
                        reset_bank,
                        heaviest_fork_failures,
                    } = select_vote_and_reset_forks(
                        &heaviest_bank,
                        heaviest_bank_on_same_voted_fork.as_ref(),
                        &ancestors,
                        &descendants,
                        &progress,
                        &mut tower,
                        &latest_validator_votes_for_frozen_banks,
                        &tbft_structs.heaviest_subtree_fork_choice,
                    );
                    select_vote_and_reset_forks_time.stop();

                    if vote_bank.is_none() {
                        Self::maybe_refresh_last_vote(
                            &mut tower,
                            &progress,
                            heaviest_bank_on_same_voted_fork,
                            &vote_account,
                            &identity_keypair,
                            &authorized_voter_keypairs.read().unwrap(),
                            &mut tracked_vote_transactions,
                            has_new_vote_been_rooted,
                            &mut last_vote_refresh_time,
                            &voting_sender,
                            wait_to_vote_slot,
                        );
                    }

                    let mut heaviest_fork_failures_time =
                        Measure::start("heaviest_fork_failures_time");
                    if tower.is_recent(heaviest_bank.slot()) && !heaviest_fork_failures.is_empty() {
                        Self::log_heaviest_fork_failures(
                            &heaviest_fork_failures,
                            &bank_forks,
                            &tower,
                            &progress,
                            &ancestors,
                            &heaviest_bank,
                            &mut last_threshold_failure_slot,
                        );
                    }
                    heaviest_fork_failures_time.stop();

                    let mut voting_time = Measure::start("voting_time");
                    // Vote on a fork
                    if let Some((ref vote_bank, ref switch_fork_decision)) = vote_bank {
                        if let Some(votable_leader) =
                            leader_schedule_cache.slot_leader_at(vote_bank.slot(), Some(vote_bank))
                        {
                            Self::log_leader_change(
                                &my_pubkey,
                                vote_bank.slot(),
                                &mut current_leader,
                                &votable_leader.id,
                            );
                        }

                        Self::handle_votable_bank(
                            vote_bank,
                            switch_fork_decision,
                            &bank_forks,
                            &mut tower,
                            &mut progress,
                            &vote_account,
                            &identity_keypair,
                            &authorized_voter_keypairs.read().unwrap(),
                            &blockstore,
                            &leader_schedule_cache,
                            &lockouts_sender,
                            snapshot_controller.as_deref(),
                            rpc_subscriptions.as_deref(),
                            &block_commitment_cache,
                            &bank_notification_sender,
                            &mut tracked_vote_transactions,
                            &mut has_new_vote_been_rooted,
                            &mut replay_timing,
                            &voting_sender,
                            &drop_bank_sender,
                            wait_to_vote_slot,
                            migration_status.as_ref(),
                            &mut tbft_structs,
                        );
                    }
                    voting_time.stop();

                    let mut reset_bank_time = Measure::start("reset_bank");
                    // Reset onto a fork
                    if let Some(reset_bank) = reset_bank {
                        if last_reset == reset_bank.last_blockhash() {
                            let reset_bank_descendants = Self::get_active_descendants(
                                reset_bank.slot(),
                                &progress,
                                &blockstore,
                            );
                            if reset_bank_descendants != last_reset_bank_descendants {
                                last_reset_bank_descendants = reset_bank_descendants;
                                poh_recorder
                                    .write()
                                    .unwrap()
                                    .update_start_bank_active_descendants(
                                        &last_reset_bank_descendants,
                                    );
                            }
                        } else {
                            info!(
                                "vote bank: {:?} reset bank: {:?}",
                                vote_bank.as_ref().map(|(b, switch_fork_decision)| (
                                    b.slot(),
                                    switch_fork_decision
                                )),
                                reset_bank.slot(),
                            );
                            let fork_progress = progress
                                .get(&reset_bank.slot())
                                .expect("bank to reset to must exist in progress map");
                            datapoint_info!(
                                "blocks_produced",
                                ("num_blocks_on_fork", fork_progress.num_blocks_on_fork, i64),
                                (
                                    "num_dropped_blocks_on_fork",
                                    fork_progress.num_dropped_blocks_on_fork,
                                    i64
                                ),
                            );

                            if my_pubkey != cluster_info.id() {
                                identity_keypair = cluster_info.keypair();
                                let my_old_pubkey = my_pubkey;
                                my_pubkey = identity_keypair.pubkey();
                                migration_status.set_pubkey(my_pubkey);

                                // Load the new identity's tower
                                tower = match Self::load_tower(
                                    tower_storage.as_ref(),
                                    &my_pubkey,
                                    &vote_account,
                                    &bank_forks,
                                ) {
                                    Ok(tower) => tower,
                                    Err(err) => {
                                        error!(
                                            "Unable to load new tower when attempting to change \
                                             identity from {my_old_pubkey} to {my_pubkey} on \
                                             set-identity, Exiting: {err}"
                                        );
                                        // drop(_exit) will set the exit flag, eventually tearing down the entire process
                                        return;
                                    }
                                };
                                // Ensure the validator can land votes with the new identity before
                                // becoming leader
                                has_new_vote_been_rooted = !wait_for_vote_to_start_leader;
                                warn!("Identity changed from {my_old_pubkey} to {my_pubkey}");
                            }

                            if !poh_controller.has_pending_message() {
                                Self::reset_poh_recorder(
                                    &my_pubkey,
                                    &blockstore,
                                    reset_bank.clone(),
                                    &mut poh_controller,
                                    &leader_schedule_cache,
                                );
                                last_reset = reset_bank.last_blockhash();
                                last_reset_bank_descendants = vec![];
                            }

                            if let Some(last_voted_slot) = tower.last_voted_slot() {
                                // If the current heaviest bank is not a descendant of the last voted slot,
                                // there must be a partition
                                partition_info.update(
                                    Self::is_partition_detected(
                                        &ancestors,
                                        last_voted_slot,
                                        heaviest_bank.slot(),
                                    ),
                                    heaviest_bank.slot(),
                                    last_voted_slot,
                                    reset_bank.slot(),
                                    heaviest_fork_failures,
                                );
                            }
                        }
                    }
                    reset_bank_time.stop();

                    let mut start_leader_time = Measure::start("start_leader_time");
                    let mut dump_then_repair_correct_slots_time =
                        Measure::start("dump_then_repair_correct_slots_time");
                    // Used for correctness check
                    let poh_bank = poh_shared_leader_state
                        .load()
                        .working_bank()
                        .map(Arc::clone);
                    // Dump any duplicate slots that have been confirmed by the network in
                    // anticipation of repairing the confirmed version of the slot.
                    //
                    // Has to be before `maybe_start_leader()`. Otherwise, `ancestors` and `descendants`
                    // will be outdated, and we cannot assume `poh_bank` will be in either of these maps.
                    Self::dump_then_repair_correct_slots(
                        &mut duplicate_slots_to_repair,
                        &mut ancestors,
                        &mut descendants,
                        &mut progress,
                        &bank_forks,
                        &blockstore,
                        poh_bank.map(|bank| bank.slot()),
                        &mut purge_repair_slot_counter,
                        &dumped_slots_sender,
                        &my_pubkey,
                        &leader_schedule_cache,
                    );
                    dump_then_repair_correct_slots_time.stop();

                    let mut retransmit_not_propagated_time =
                        Measure::start("retransmit_not_propagated_time");
                    Self::retransmit_latest_unpropagated_leader_slot(
                        &poh_recorder,
                        &retransmit_slots_sender,
                        &mut progress,
                    );
                    retransmit_not_propagated_time.stop();

                    // From this point on, its not safe to use ancestors/descendants since maybe_start_leader
                    // may add a bank that will not included in either of these maps.
                    drop(ancestors);
                    drop(descendants);
                    if !tpu_has_bank
                        && !poh_controller.has_pending_message()
                        && let Some(poh_slot) = Self::maybe_start_leader(
                            &my_pubkey,
                            &bank_forks,
                            &poh_recorder,
                            &mut poh_controller,
                            &leader_schedule_cache,
                            rpc_subscriptions.as_deref(),
                            &slot_status_notifier,
                            &mut progress,
                            &retransmit_slots_sender,
                            &mut skipped_slots_info,
                            &banking_tracer,
                            has_new_vote_been_rooted,
                            migration_status.as_ref(),
                        )
                    {
                        Self::log_leader_change(
                            &my_pubkey,
                            poh_slot,
                            &mut current_leader,
                            &my_pubkey,
                        );
                    }
                    start_leader_time.stop();

                    replay_timing.update_non_alpenglow(
                        collect_frozen_banks_time.as_us(),
                        compute_bank_stats_time.as_us(),
                        select_vote_and_reset_forks_time.as_us(),
                        reset_bank_time.as_us(),
                        voting_time.as_us(),
                        select_forks_time.as_us(),
                        compute_slot_stats_time.as_us(),
                        heaviest_fork_failures_time.as_us(),
                        u64::try_from(new_frozen_slots.len()).expect(
                            "something is very wrong, froze more than u64::MAX banks at once",
                        ),
                        process_ancestor_hashes_duplicate_slots_time.as_us(),
                        process_duplicate_confirmed_slots_time.as_us(),
                        process_unfrozen_gossip_verified_vote_hashes_time.as_us(),
                        process_popular_pruned_forks_time.as_us(),
                        process_duplicate_slots_time.as_us(),
                        dump_then_repair_correct_slots_time.as_us(),
                        retransmit_not_propagated_time.as_us(),
                        start_leader_time.as_us(),
                    );
                }

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

                replay_timing.update_common(
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
        migration_status: &MigrationStatus,
        bank_forks: &RwLock<BankForks>,
        my_pubkey: &Pubkey,
        leader_schedule_cache: &LeaderScheduleCache,
        optimistic_parent_sender: &Sender<LeaderWindowInfo>,
        optimistic_parent_receiver: &Receiver<LeaderWindowInfo>,
        replay_highest_frozen: &ReplayHighestFrozen,
        highest_frozen_slot: &mut Slot,
    ) {
        let flh_candidate_banks = {
            let bank_forks_r = bank_forks.read().unwrap();
            new_frozen_slots
                .iter()
                .filter_map(|slot| bank_forks_r.get(*slot))
                .filter(|bank| {
                    bank.feature_set.snapshot().alpenglow_fast_leader_handover
                        && migration_status.should_allow_block_markers(bank.slot())
                })
                .collect_vec()
        };
        for bank in flh_candidate_banks {
            Self::maybe_notify_of_optimistic_parent(
                &bank,
                my_pubkey,
                leader_schedule_cache,
                optimistic_parent_sender,
                optimistic_parent_receiver,
            );
        }

        if let Some(highest) = new_frozen_slots.iter().max()
            && *highest > *highest_frozen_slot
        {
            *highest_frozen_slot = *highest;
            let mut l_highest_frozen = replay_highest_frozen.highest_frozen_slot.lock().unwrap();
            // Let the block creation loop know about this new frozen slot
            *l_highest_frozen = *highest;
            replay_highest_frozen.freeze_notification.notify_one();
        }
    }

    /// Enables alpenglow
    /// - Clears any in progress leader blocks
    /// - Clears any TowerBFT blocks past the genesis block
    /// - Shutdown poh
    /// - Start block creation loop and Votor
    ///
    /// Should only be called if we're in `ReadyToEnable`
    #[allow(clippy::too_many_arguments)]
    fn enable_alpenglow(
        exit: &AtomicBool,
        my_pubkey: &Pubkey,
        migration_status: &MigrationStatus,
        bank_forks: &RwLock<BankForks>,
        blockstore: &Blockstore,
        poh_controller: &mut PohController,
        shared_leader_state: &SharedLeaderState,
        leader_schedule_cache: &LeaderScheduleCache,
        ancestors: &mut HashMap<Slot, HashSet<Slot>>,
        descendants: &mut HashMap<Slot, HashSet<Slot>>,
        progress: &mut ProgressMap,
    ) {
        let root_bank = bank_forks.read().unwrap().root_bank();

        let genesis_block = migration_status
            .genesis_block()
            .expect("Must be ready to enable");
        info!(
            "{my_pubkey} Alpenglow migration: Alpenglow genesis vote has succeeded enabling \
             alpenglow. Genesis block {genesis_block:?}"
        );

        let genesis_bank = bank_forks.read().unwrap().get(genesis_block.slot).expect(
            "{my_pubkey}: Attempting to enable alpenglow before receiving the genesis block",
        );
        assert!(genesis_bank.is_frozen());

        if genesis_bank.block_id() != Some(genesis_block.block_id) {
            panic!(
                "{my_pubkey}: Attempting to enable alpenglow but we have the wrong version of the \
                 genesis block our version: ({}, {:?}), certified version ({genesis_block:?})",
                genesis_block.slot,
                genesis_bank.block_id()
            );
        }

        // Reset poh to the genesis block. This has to be done first before we can clear banks to
        // avoid any inflight issues with transaction recording.
        Self::reset_poh_recorder(
            my_pubkey,
            blockstore,
            genesis_bank,
            poh_controller,
            leader_schedule_cache,
        );
        while poh_controller.has_pending_message()
            || shared_leader_state.load().working_bank().is_some()
        {
            std::hint::spin_loop();
        }

        // Purge all slots greater than the genesis slot from AccountsDB & blockstore
        let slots_to_purge: Vec<Slot> = bank_forks
            .read()
            .unwrap()
            .banks()
            .keys()
            .filter_map(|slot| (*slot > genesis_block.slot).then_some(*slot))
            .collect();
        for slot in slots_to_purge.into_iter() {
            info!("{my_pubkey} Alpenglow migration: Purging poh block in slot {slot}");
            Self::purge_unconfirmed_slot(
                slot,
                ancestors,
                descendants,
                progress,
                root_bank.as_ref(),
                bank_forks,
                blockstore,
            );
        }

        // Purge any partial slots greater than the genesis slot
        let start_slot = genesis_block.slot + 1;
        let end_slot = blockstore
            .highest_slot()
            .unwrap()
            .expect("Highest slot must be present as blockstore is non-empty");
        if end_slot >= start_slot {
            info!(
                "{my_pubkey} Alpenglow migration: Purging shreds {start_slot} to {end_slot} from \
                 blockstore"
            );
            blockstore.clear_unconfirmed_slots(start_slot, end_slot);
        }

        migration_status.enable_alpenglow(exit);

        assert!(migration_status.is_alpenglow_enabled());
        datapoint_info!(
            "migration-complete",
            ("genesis_slot", genesis_block.slot as i64, i64),
        );
    }

    /// If we have an eligible genesis block, send out a genesis vote
    /// Returns false if no eligible block was found
    fn maybe_send_genesis_vote(
        my_shred_version: u16,
        migration_status: &MigrationStatus,
        bank_forks: &RwLock<BankForks>,
        vote_account: Pubkey,
        identity_keypair: &Arc<Keypair>,
        authorized_voter_keypairs: &Arc<std::sync::RwLock<Vec<Arc<Keypair>>>>,
        own_message_sender: &Sender<SigVerifiedBatch>,
        bls_sender: &Sender<BLSOp>,
    ) -> bool {
        let Some(block) = migration_status.eligible_genesis_block() else {
            // We have not yet discovered the genesis block
            return false;
        };

        let vote = Vote::new_genesis_vote(block);
        match voting_utils::generate_vote_tx(
            vote,
            bank_forks.read().unwrap().root_bank().as_ref(),
            vote_account,
            my_shred_version,
            identity_keypair,
            authorized_voter_keypairs,
            None,
            &mut HashMap::new(),
        ) {
            GenerateVoteTxResult::Vote(vote_msg) => {
                // Send vote to ConsensusPool and rest of cluster
                warn!(
                    "{} Alpenglow migration: Casting genesis vote for ({block:?})",
                    identity_keypair.pubkey()
                );
                // Unlikely that these channels are backed up, but even so we have refresh logic on the genesis vote
                let _ =
                    own_message_sender.try_send(SigVerifiedBatch::Votes(vec![vote_msg.clone()]));
                let _ = bls_sender.try_send(BLSOp::PushVote {
                    vote: Arc::new(vote_msg),
                });
            }
            e => {
                warn!(
                    "{} Alpenglow migration: Unable to send genesis vote for {block:?}: {e:?}",
                    identity_keypair.pubkey()
                );
            }
        }
        true
    }

    /// Loads the tower from `tower_storage` with identity `node_pubkey`.
    ///
    /// If the tower is missing or too old, a tower is constructed from bank forks.
    fn load_tower(
        tower_storage: &dyn TowerStorage,
        node_pubkey: &Pubkey,
        vote_account: &Pubkey,
        bank_forks: &RwLock<BankForks>,
    ) -> Result<Tower, TowerError> {
        let tower = Tower::restore(tower_storage, node_pubkey).and_then(|restored_tower| {
            let root_bank = bank_forks.read().unwrap().root_bank();
            let slot_history = root_bank
                .get_slot_history()
                .expect("slot history must exist");
            restored_tower.adjust_lockouts_after_replay(root_bank.slot(), &slot_history)
        });
        match tower {
            Ok(tower) => Ok(tower),
            Err(err) if err.is_file_missing() => {
                warn!(
                    "Failed to load tower, file missing for {node_pubkey}: {err}. Creating a new \
                     tower from bankforks."
                );
                Ok(Tower::new_from_bankforks(
                    &bank_forks.read().unwrap(),
                    node_pubkey,
                    vote_account,
                ))
            }
            Err(err) if err.is_too_old() => {
                warn!(
                    "Failed to load tower, too old for {node_pubkey}: {err}. Creating a new tower \
                     from bankforks."
                );
                Ok(Tower::new_from_bankforks(
                    &bank_forks.read().unwrap(),
                    node_pubkey,
                    vote_account,
                ))
            }
            Err(err) => Err(err),
        }
    }

    fn maybe_retransmit_unpropagated_slots(
        metric_name: &'static str,
        retransmit_slots_sender: &Sender<Slot>,
        progress: &mut ProgressMap,
        latest_leader_slot: Slot,
    ) {
        let first_leader_group_slot = first_of_consecutive_leader_slots(latest_leader_slot);

        for slot in first_leader_group_slot..=latest_leader_slot {
            let is_propagated = progress.is_propagated(slot);
            if let Some(retransmit_info) = progress.get_retransmit_info_mut(slot)
                && !is_propagated.expect(
                    "presence of retransmit_info ensures that propagation status is present",
                )
            {
                if retransmit_info.reached_retransmit_threshold() {
                    info!(
                        "Retrying retransmit: latest_leader_slot={latest_leader_slot} slot={slot} \
                         retransmit_info={retransmit_info:?}",
                    );
                    datapoint_info!(
                        metric_name,
                        ("latest_leader_slot", latest_leader_slot, i64),
                        ("slot", slot, i64),
                        ("retry_iteration", retransmit_info.retry_iteration, i64),
                    );
                    let _ = retransmit_slots_sender.send(slot);
                    retransmit_info.increment_retry_iteration();
                } else {
                    debug!("Bypass retransmit of slot={slot} retransmit_info={retransmit_info:?}");
                }
            }
        }
    }

    fn retransmit_latest_unpropagated_leader_slot(
        poh_recorder: &Arc<RwLock<PohRecorder>>,
        retransmit_slots_sender: &Sender<Slot>,
        progress: &mut ProgressMap,
    ) {
        let start_slot = poh_recorder.read().unwrap().start_slot();

        // It is possible that bank corresponding to `start_slot` has been
        // dumped, so we need to double check it exists before proceeding
        if !progress.contains(&start_slot) {
            warn!(
                "Poh start slot {start_slot}, is missing from progress map. This indicates that \
                 we are in the middle of a dump and repair. Skipping retransmission of \
                 unpropagated leader slots"
            );
            return;
        }

        if let (false, Some(latest_leader_slot)) =
            progress.get_leader_propagation_slot_must_exist(start_slot)
        {
            debug!(
                "Slot not propagated: start_slot={start_slot} \
                 latest_leader_slot={latest_leader_slot}"
            );
            Self::maybe_retransmit_unpropagated_slots(
                "replay_stage-retransmit-timing-based",
                retransmit_slots_sender,
                progress,
                latest_leader_slot,
            );
        }
    }

    fn is_partition_detected(
        ancestors: &HashMap<Slot, HashSet<Slot>>,
        last_voted_slot: Slot,
        heaviest_slot: Slot,
    ) -> bool {
        last_voted_slot != heaviest_slot
            && !ancestors
                .get(&heaviest_slot)
                .map(|ancestors| ancestors.contains(&last_voted_slot))
                .unwrap_or(true)
    }

    fn get_active_descendants(
        slot: Slot,
        progress: &ProgressMap,
        blockstore: &Blockstore,
    ) -> Vec<Slot> {
        let Some(slot_meta) = blockstore.meta(slot).ok().flatten() else {
            return vec![];
        };

        slot_meta
            .next_slots
            .iter()
            .filter(|slot| !progress.is_dead(**slot).unwrap_or_default())
            .copied()
            .collect()
    }

    fn initialize_progress_and_fork_choice_with_locked_bank_forks(
        bank_forks: &RwLock<BankForks>,
        my_pubkey: &Pubkey,
        vote_account: &Pubkey,
        blockstore: &Blockstore,
    ) -> (ProgressMap, HeaviestSubtreeForkChoice) {
        let (root_bank, frozen_banks, duplicate_slot_hashes) = {
            let bank_forks = bank_forks.read().unwrap();
            let root_bank = bank_forks.root_bank();
            let duplicate_slots = blockstore
                // It is important that the root bank is not marked as duplicate on initialization.
                // Although this bank could contain a duplicate proof, the fact that it was rooted
                // either during a previous run or artificially means that we should ignore any
                // duplicate proofs for the root slot, thus we start consuming duplicate proofs
                // from the root slot + 1
                .duplicate_slots_iterator(root_bank.slot().saturating_add(1))
                .unwrap();
            let duplicate_slot_hashes = duplicate_slots.filter_map(|slot| {
                let bank = bank_forks.get(slot)?;
                Some((slot, bank.hash()))
            });
            (
                root_bank,
                bank_forks
                    .frozen_banks()
                    .map(|(_slot, bank)| bank)
                    .collect(),
                duplicate_slot_hashes.collect::<Vec<(Slot, Hash)>>(),
            )
        };

        Self::initialize_progress_and_fork_choice(
            &root_bank,
            frozen_banks,
            my_pubkey,
            vote_account,
            duplicate_slot_hashes,
        )
    }

    pub fn initialize_progress_and_fork_choice(
        root_bank: &Bank,
        mut frozen_banks: Vec<Arc<Bank>>,
        my_pubkey: &Pubkey,
        vote_account: &Pubkey,
        duplicate_slot_hashes: Vec<(Slot, Hash)>,
    ) -> (ProgressMap, HeaviestSubtreeForkChoice) {
        let mut progress = ProgressMap::default();

        frozen_banks.sort_by_key(|bank| bank.slot());

        // Initialize progress map with any root banks
        for bank in &frozen_banks {
            let prev_leader_slot = progress.get_bank_prev_leader_slot(bank);
            progress.insert(
                bank.slot(),
                ForkProgress::new_from_bank(
                    bank,
                    my_pubkey,
                    vote_account,
                    prev_leader_slot,
                    0,
                    0,
                    None,
                ),
            );
        }
        let root = root_bank.slot();
        let mut heaviest_subtree_fork_choice = HeaviestSubtreeForkChoice::new_from_frozen_banks(
            (root, root_bank.hash()),
            &frozen_banks,
        );

        for slot_hash in duplicate_slot_hashes {
            heaviest_subtree_fork_choice.mark_fork_invalid_candidate(&slot_hash);
        }

        (progress, heaviest_subtree_fork_choice)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn dump_then_repair_correct_slots(
        duplicate_slots_to_repair: &mut DuplicateSlotsToRepair,
        ancestors: &mut HashMap<Slot, HashSet<Slot>>,
        descendants: &mut HashMap<Slot, HashSet<Slot>>,
        progress: &mut ProgressMap,
        bank_forks: &RwLock<BankForks>,
        blockstore: &Blockstore,
        poh_bank_slot: Option<Slot>,
        purge_repair_slot_counter: &mut PurgeRepairSlotCounter,
        dumped_slots_sender: &DumpedSlotsSender,
        my_pubkey: &Pubkey,
        leader_schedule_cache: &LeaderScheduleCache,
    ) {
        if duplicate_slots_to_repair.is_empty() {
            return;
        }

        let root_bank = bank_forks.read().unwrap().root_bank();
        let mut dumped = vec![];
        // TODO: handle if alternate version of descendant also got confirmed after ancestor was
        // confirmed, what happens then? Should probably keep track of dumped list and skip things
        // in `duplicate_slots_to_repair` that have already been dumped. Add test.
        duplicate_slots_to_repair.retain(|duplicate_slot, correct_hash| {
            // Should not dump duplicate slots if there is currently a poh bank building
            // on top of that slot, as BankingStage might still be referencing/touching that state
            // concurrently.
            // Luckily for us, because the fork choice rule removes duplicate slots from fork
            // choice, and this function is called after:
            // 1) We have picked a bank to reset to in `select_vote_and_reset_forks()`
            // 2) And also called `reset_poh_recorder()`
            // Then we should have reset to a fork that doesn't include the duplicate block,
            // which means any working bank in PohRecorder that was built on that duplicate fork
            // should have been cleared as well. However, if there is some violation of this guarantee,
            // then log here
            let is_poh_building_on_duplicate_fork = poh_bank_slot
                .map(|poh_bank_slot| {
                    ancestors
                        .get(&poh_bank_slot)
                        .expect("Poh bank should exist in BankForks and thus in ancestors map")
                        .contains(duplicate_slot)
                })
                .unwrap_or(false);

            let did_dump_repair = {
                if !is_poh_building_on_duplicate_fork {
                    let frozen_hash = bank_forks.read().unwrap().bank_hash(*duplicate_slot);
                    if let Some(frozen_hash) = frozen_hash {
                        if frozen_hash == *correct_hash {
                            warn!(
                                "Trying to dump slot {} with correct_hash {}",
                                *duplicate_slot, *correct_hash
                            );
                            return false;
                        } else if frozen_hash == Hash::default()
                            && !progress.is_dead(*duplicate_slot).expect(
                                "If slot exists in BankForks must exist in the progress map",
                            )
                        {
                            warn!(
                                "Trying to dump unfrozen slot {} that is not dead",
                                *duplicate_slot
                            );
                            return false;
                        }
                    } else {
                        warn!(
                            "Dumping slot {} which does not exist in bank forks (possibly pruned)",
                            *duplicate_slot
                        );
                    }

                    // Should not dump slots for which we were the leader
                    if Some(*my_pubkey)
                        == leader_schedule_cache
                            .slot_leader_at(*duplicate_slot, None)
                            .map(|leader| leader.id)
                    {
                        if let Some(bank) = bank_forks.read().unwrap().get(*duplicate_slot) {
                            bank_hash_details::write_bank_hash_details_file(&bank)
                                .map_err(|err| {
                                    warn!("Unable to write bank hash details file: {err}");
                                })
                                .ok();
                        } else {
                            warn!(
                                "Unable to get bank for slot {duplicate_slot} from bank forks \
                                 while attempting to write bank hash details file"
                            );
                        }
                        panic!(
                            "We are attempting to dump a block that we produced. This indicates \
                             that we are producing duplicate blocks, or that there is a bug in \
                             our runtime/replay code which causes us to compute different bank \
                             hashes than the rest of the cluster. We froze slot {duplicate_slot} \
                             with hash {frozen_hash:?} while the cluster hash is {correct_hash}"
                        );
                    }

                    let attempt_no = purge_repair_slot_counter
                        .entry(*duplicate_slot)
                        .and_modify(|x| *x += 1)
                        .or_insert(1);
                    if *attempt_no > MAX_REPAIR_RETRY_LOOP_ATTEMPTS {
                        panic!(
                            "We have tried to repair duplicate slot: {duplicate_slot} more than \
                             {MAX_REPAIR_RETRY_LOOP_ATTEMPTS} times and are unable to freeze a \
                             block with bankhash {correct_hash}, instead we have a block with \
                             bankhash {frozen_hash:?}. This is most likely a bug in the runtime. \
                             At this point manual intervention is needed to make progress. Exiting"
                        );
                    }

                    Self::purge_unconfirmed_slot(
                        *duplicate_slot,
                        ancestors,
                        descendants,
                        progress,
                        &root_bank,
                        bank_forks,
                        blockstore,
                    );

                    dumped.push((*duplicate_slot, *correct_hash));

                    warn!(
                        "Notifying repair service to repair duplicate slot: {}, attempt {}",
                        *duplicate_slot, *attempt_no,
                    );
                    true
                } else {
                    warn!(
                        "PoH bank for slot {} is building on duplicate slot {}",
                        poh_bank_slot.unwrap(),
                        duplicate_slot
                    );
                    false
                }
            };

            // If we dumped/repaired, then no need to keep the slot in the set of pending work
            !did_dump_repair
        });

        // Notify repair of the dumped slots along with the correct hash
        trace!("Dumped {} slots", dumped.len());
        dumped_slots_sender.send(dumped).unwrap();
    }

    #[allow(clippy::too_many_arguments)]
    fn process_ancestor_hashes_duplicate_slots(
        pubkey: &Pubkey,
        blockstore: &Blockstore,
        ancestor_duplicate_slots_receiver: &AncestorDuplicateSlotsReceiver,
        duplicate_slots_tracker: &mut DuplicateSlotsTracker,
        duplicate_confirmed_slots: &DuplicateConfirmedSlots,
        epoch_slots_frozen_slots: &mut EpochSlotsFrozenSlots,
        progress: &ProgressMap,
        fork_choice: &mut HeaviestSubtreeForkChoice,
        bank_forks: &RwLock<BankForks>,
        duplicate_slots_to_repair: &mut DuplicateSlotsToRepair,
        ancestor_hashes_replay_update_sender: &AncestorHashesReplayUpdateSender,
        purge_repair_slot_counter: &mut PurgeRepairSlotCounter,
    ) {
        let root = bank_forks.read().unwrap().root();
        for AncestorDuplicateSlotToRepair {
            slot_to_repair: (epoch_slots_frozen_slot, epoch_slots_frozen_hash),
            request_type,
        } in ancestor_duplicate_slots_receiver.try_iter()
        {
            warn!(
                "{} ReplayStage notified of duplicate slot from ancestor hashes service but we \
                 observed as {}: {:?}",
                pubkey,
                if request_type.is_pruned() {
                    "pruned"
                } else {
                    "dead"
                },
                (epoch_slots_frozen_slot, epoch_slots_frozen_hash),
            );
            let epoch_slots_frozen_state = EpochSlotsFrozenState::new_from_state(
                epoch_slots_frozen_slot,
                epoch_slots_frozen_hash,
                duplicate_confirmed_slots,
                fork_choice,
                || progress.is_dead(epoch_slots_frozen_slot).unwrap_or(false),
                || {
                    bank_forks
                        .read()
                        .unwrap()
                        .get(epoch_slots_frozen_slot)
                        .map(|b| b.hash())
                },
                request_type.is_pruned(),
            );
            check_slot_agrees_with_cluster(
                epoch_slots_frozen_slot,
                root,
                blockstore,
                duplicate_slots_tracker,
                epoch_slots_frozen_slots,
                fork_choice,
                duplicate_slots_to_repair,
                ancestor_hashes_replay_update_sender,
                purge_repair_slot_counter,
                SlotStateUpdate::EpochSlotsFrozen(epoch_slots_frozen_state),
            );
        }
    }

    fn purge_unconfirmed_slot(
        slot_to_purge: Slot,
        ancestors: &mut HashMap<Slot, HashSet<Slot>>,
        descendants: &mut HashMap<Slot, HashSet<Slot>>,
        progress: &mut ProgressMap,
        root_bank: &Bank,
        bank_forks: &RwLock<BankForks>,
        blockstore: &Blockstore,
    ) {
        warn!("purging slot {slot_to_purge}");

        // Doesn't need to be root bank, just needs a common bank to
        // access the status cache and accounts
        let slot_descendants = descendants.get(&slot_to_purge).cloned();
        if slot_descendants.is_none() {
            // Root has already moved past this slot, no need to purge it
            if root_bank.slot() <= slot_to_purge {
                blockstore.clear_unconfirmed_slot(slot_to_purge);
            }

            return;
        }

        // Clear the ancestors/descendants map to keep them
        // consistent
        let slot_descendants = slot_descendants.unwrap();
        Self::purge_ancestors_descendants(slot_to_purge, &slot_descendants, ancestors, descendants);

        let banks_to_remove: Vec<_> = {
            let bank_forks = bank_forks.read().unwrap();
            slot_descendants
                .iter()
                .chain(std::iter::once(&slot_to_purge))
                .filter_map(|slot| bank_forks.get_with_scheduler(*slot))
                .collect()
        };
        for bank in banks_to_remove {
            let _ = bank.wait_for_completed_scheduler();
        }

        // Grab the Slot and BankId's of the banks we need to purge, then clear the banks
        // from BankForks
        let (slots_to_purge, removed_banks): (Vec<(Slot, BankId)>, Vec<BankWithScheduler>) = {
            let mut w_bank_forks = bank_forks.write().unwrap();
            w_bank_forks.dump_slots(
                slot_descendants
                    .iter()
                    .chain(std::iter::once(&slot_to_purge)),
                true,
            )
        };

        // Clear the accounts for these slots so that any ongoing RPC scans fail.
        // These have to be atomically cleared together in the same batch, in order
        // to prevent RPC from seeing inconsistent results in scans.
        root_bank.remove_unrooted_slots(&slots_to_purge);

        // Once the slots above have been purged, now it's safe to remove the banks from
        // BankForks, allowing the Bank::drop() purging to run and not race with the
        // `remove_unrooted_slots()` call.
        drop(removed_banks);

        for (slot, slot_id) in slots_to_purge {
            // Clear the slot signatures from status cache for this slot.
            // TODO: What about RPC queries that had already cloned the Bank for this slot
            // and are looking up the signature for this slot?
            root_bank.clear_slot_signatures(slot);

            // Remove cached entries of the programs that were deployed in this slot.
            root_bank.prune_program_cache_by_deployment_slot(slot);

            if let Some(bank_hash) = blockstore.get_bank_hash(slot) {
                // If a descendant was successfully replayed and chained from a duplicate it must
                // also be a duplicate. In this case we *need* to repair it, so we clear from
                // blockstore.
                warn!(
                    "purging duplicate descendant: {slot} with slot_id {slot_id} and bank hash \
                     {bank_hash}, of slot {slot_to_purge}"
                );
                // Clear the slot-related data in blockstore. This will:
                // 1) Clear old shreds allowing new ones to be inserted
                // 2) Clear the "dead" flag allowing ReplayStage to start replaying
                // this slot
                blockstore.clear_unconfirmed_slot(slot);
            } else if slot == slot_to_purge {
                warn!("purging duplicate slot: {slot} with slot_id {slot_id}");
                blockstore.clear_unconfirmed_slot(slot);
            } else {
                // If a descendant was unable to replay and chained from a duplicate, it is not
                // necessary to repair it. It is most likely that this block is fine, and will
                // replay on successful repair of the parent. If this block is also a duplicate, it
                // will be handled in the next round of repair/replay - so we just clear the dead
                // flag for now.
                warn!(
                    "not purging descendant {slot} of slot {slot_to_purge} as it is dead. \
                     resetting dead flag instead"
                );
                // Clear the "dead" flag allowing ReplayStage to start replaying
                // this slot once the parent is repaired
                blockstore.remove_dead_slot(slot).unwrap();
            }

            // Clear the progress map of these forks
            let _ = progress.remove(&slot);
        }
    }

    // Purge given slot and all its descendants from the `ancestors` and
    // `descendants` structures so that they're consistent with `BankForks`
    // and the `progress` map.
    fn purge_ancestors_descendants(
        slot: Slot,
        slot_descendants: &HashSet<Slot>,
        ancestors: &mut HashMap<Slot, HashSet<Slot>>,
        descendants: &mut HashMap<Slot, HashSet<Slot>>,
    ) {
        if !ancestors.contains_key(&slot) {
            // Slot has already been purged
            return;
        }

        // Purge this slot from each of its ancestors' `descendants` maps
        for a in ancestors
            .get(&slot)
            .expect("must exist based on earlier check")
        {
            descendants
                .get_mut(a)
                .expect("If exists in ancestor map must exist in descendants map")
                .retain(|d| *d != slot && !slot_descendants.contains(d));
        }
        ancestors
            .remove(&slot)
            .expect("must exist based on earlier check");

        // Purge all the descendants of this slot from both maps
        for descendant in slot_descendants {
            ancestors.remove(descendant).expect("must exist");
            descendants
                .remove(descendant)
                .expect("must exist based on earlier check");
        }
        descendants
            .remove(&slot)
            .expect("must exist based on earlier check");
    }

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

        if bank_forks.read().unwrap().block_id(block.slot) == Some(block.block_id) {
            // Nothing to switch
            *pending_switch = None;
            return Ok(());
        }

        // Check if we have received the block and all of its ancestors and collect the ones we
        // need to switch out
        let mut ancestor_slot = block.slot;
        let mut ancestor_block_id = block.block_id;
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

        let slots_to_clear = bank_forks.read().unwrap().slots_to_clear(
            blocks_to_switch
                .iter()
                .map(|(slot, _)| *slot)
                .chain(original_dead_slots_to_clear.iter().copied()),
        );

        info!("{my_pubkey}: Clearing banks for switching and descendants: {slots_to_clear:?}");
        Self::clear_banks(
            &slots_to_clear,
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

    /// For slots to clear, clear the bank from progress, bank forks, and recycle the async verification
    fn clear_banks(
        slots_to_clear: &BTreeSet<Slot>,
        bank_forks: &RwLock<BankForks>,
        progress: &mut ProgressMap,
        async_verification_freelist: &mut Vec<AsyncVerificationProgress>,
    ) {
        if slots_to_clear.is_empty() {
            return;
        }

        // Wait for async verify to complete
        for slot in slots_to_clear {
            if let Some(replay_progress) = progress.remove(slot) {
                let mut w_replay_progress = replay_progress.replay_progress.write().unwrap();
                let _ = w_replay_progress.wait_for_all_verification_results(&mut 0, &mut 0);
                Self::recycle_async_verification(
                    async_verification_freelist,
                    w_replay_progress.take_async_verification(),
                );
            }
        }

        let banks_to_remove = {
            let bank_forks = bank_forks.read().unwrap();
            slots_to_clear
                .iter()
                .filter_map(|slot| bank_forks.get_with_scheduler(*slot))
                .collect::<Vec<_>>()
        };

        // Wait for any in progress execution
        for bank in banks_to_remove {
            let _ = bank.wait_for_completed_scheduler();
        }

        // Dump the banks from bank forks
        let (root_bank, slots_to_purge, removed_banks) = {
            let mut w_bank_forks = bank_forks.write().unwrap();
            let slots_to_clear = slots_to_clear
                .iter()
                .copied()
                .filter(|slot| w_bank_forks.get(*slot).is_some())
                .collect::<BTreeSet<_>>();
            if slots_to_clear.is_empty() {
                return;
            }

            let root_bank = w_bank_forks.root_bank();
            let (slots_to_purge, removed_banks) =
                w_bank_forks.dump_slots(slots_to_clear.iter(), false);
            (root_bank, slots_to_purge, removed_banks)
        };

        // Clear the accounts for these slots so that any ongoing RPC scans fail.
        // These have to be atomically cleared together in the same batch, in order
        // to prevent RPC from seeing inconsistent results in scans.
        root_bank.remove_unrooted_slots(&slots_to_purge);

        // Once the slots above have been purged, now it's safe to remove the banks from
        // BankForks, allowing the Bank::drop() purging to run and not race with the
        // `remove_unrooted_slots()` call.
        drop(removed_banks);

        // Clear slot signatures from status cache and programs from program cache
        for (slot, _) in slots_to_purge {
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
    fn process_popular_pruned_forks(
        popular_pruned_forks_receiver: &PopularPrunedForksReceiver,
        blockstore: &Blockstore,
        duplicate_slots_tracker: &mut DuplicateSlotsTracker,
        epoch_slots_frozen_slots: &mut EpochSlotsFrozenSlots,
        bank_forks: &RwLock<BankForks>,
        fork_choice: &mut HeaviestSubtreeForkChoice,
        duplicate_slots_to_repair: &mut DuplicateSlotsToRepair,
        ancestor_hashes_replay_update_sender: &AncestorHashesReplayUpdateSender,
        purge_repair_slot_counter: &mut PurgeRepairSlotCounter,
    ) {
        let root = bank_forks.read().unwrap().root();
        for new_popular_pruned_slots in popular_pruned_forks_receiver.try_iter() {
            for new_popular_pruned_slot in new_popular_pruned_slots {
                if new_popular_pruned_slot <= root {
                    continue;
                }
                check_slot_agrees_with_cluster(
                    new_popular_pruned_slot,
                    root,
                    blockstore,
                    duplicate_slots_tracker,
                    epoch_slots_frozen_slots,
                    fork_choice,
                    duplicate_slots_to_repair,
                    ancestor_hashes_replay_update_sender,
                    purge_repair_slot_counter,
                    SlotStateUpdate::PopularPrunedFork,
                );
            }
        }
    }

    // Check for any newly duplicate confirmed slots by the cluster.
    // This only tracks duplicate slot confirmations on the exact
    // single slots and does not account for votes on their descendants. Used solely
    // for duplicate slot recovery.
    #[allow(clippy::too_many_arguments)]
    fn process_duplicate_confirmed_slots(
        duplicate_confirmed_slots_receiver: &DuplicateConfirmedSlotsReceiver,
        blockstore: &Blockstore,
        duplicate_slots_tracker: &mut DuplicateSlotsTracker,
        duplicate_confirmed_slots: &mut DuplicateConfirmedSlots,
        epoch_slots_frozen_slots: &mut EpochSlotsFrozenSlots,
        bank_forks: &RwLock<BankForks>,
        progress: &ProgressMap,
        fork_choice: &mut HeaviestSubtreeForkChoice,
        duplicate_slots_to_repair: &mut DuplicateSlotsToRepair,
        ancestor_hashes_replay_update_sender: &AncestorHashesReplayUpdateSender,
        purge_repair_slot_counter: &mut PurgeRepairSlotCounter,
    ) {
        let root = bank_forks.read().unwrap().root();
        for new_duplicate_confirmed_slots in duplicate_confirmed_slots_receiver.try_iter() {
            for (confirmed_slot, duplicate_confirmed_hash) in new_duplicate_confirmed_slots {
                if confirmed_slot <= root {
                    continue;
                } else if let Some(prev_hash) =
                    duplicate_confirmed_slots.insert(confirmed_slot, duplicate_confirmed_hash)
                {
                    // This assertion is intentional - it is not possible to split the cluster to get 52% on two versions
                    // without a massive turbine failure
                    assert_eq!(
                        prev_hash, duplicate_confirmed_hash,
                        "Additional duplicate confirmed notification for slot {confirmed_slot} \
                         with a different hash"
                    );
                    // Already processed this signal
                    continue;
                }

                let duplicate_confirmed_state = DuplicateConfirmedState::new_from_state(
                    duplicate_confirmed_hash,
                    || progress.is_dead(confirmed_slot).unwrap_or(false),
                    || bank_forks.read().unwrap().bank_hash(confirmed_slot),
                );
                check_slot_agrees_with_cluster(
                    confirmed_slot,
                    root,
                    blockstore,
                    duplicate_slots_tracker,
                    epoch_slots_frozen_slots,
                    fork_choice,
                    duplicate_slots_to_repair,
                    ancestor_hashes_replay_update_sender,
                    purge_repair_slot_counter,
                    SlotStateUpdate::DuplicateConfirmed(duplicate_confirmed_state),
                );
            }
        }
    }

    fn process_gossip_verified_vote_hashes(
        gossip_verified_vote_hash_receiver: &GossipVerifiedVoteHashReceiver,
        unfrozen_gossip_verified_vote_hashes: &mut UnfrozenGossipVerifiedVoteHashes,
        heaviest_subtree_fork_choice: &HeaviestSubtreeForkChoice,
        latest_validator_votes_for_frozen_banks: &mut LatestValidatorVotesForFrozenBanks,
    ) {
        for (pubkey, slot, hash) in gossip_verified_vote_hash_receiver.try_iter() {
            let is_frozen = heaviest_subtree_fork_choice.contains_block(&(slot, hash));
            // cluster_info_vote_listener will ensure it doesn't push duplicates
            unfrozen_gossip_verified_vote_hashes.add_vote(
                pubkey,
                slot,
                hash,
                is_frozen,
                latest_validator_votes_for_frozen_banks,
            )
        }
    }

    // Checks for and handle forks with duplicate slots.
    #[allow(clippy::too_many_arguments)]
    fn process_duplicate_slots(
        blockstore: &Blockstore,
        duplicate_slots_receiver: &DuplicateSlotReceiver,
        duplicate_slots_tracker: &mut DuplicateSlotsTracker,
        duplicate_confirmed_slots: &DuplicateConfirmedSlots,
        epoch_slots_frozen_slots: &mut EpochSlotsFrozenSlots,
        bank_forks: &RwLock<BankForks>,
        progress: &ProgressMap,
        fork_choice: &mut HeaviestSubtreeForkChoice,
        duplicate_slots_to_repair: &mut DuplicateSlotsToRepair,
        ancestor_hashes_replay_update_sender: &AncestorHashesReplayUpdateSender,
        purge_repair_slot_counter: &mut PurgeRepairSlotCounter,
    ) {
        let new_duplicate_slots: Vec<Slot> = duplicate_slots_receiver.try_iter().collect();
        let (root_slot, bank_hashes) = {
            let r_bank_forks = bank_forks.read().unwrap();
            let bank_hashes: Vec<Option<Hash>> = new_duplicate_slots
                .iter()
                .map(|duplicate_slot| r_bank_forks.bank_hash(*duplicate_slot))
                .collect();

            (r_bank_forks.root(), bank_hashes)
        };
        for (duplicate_slot, bank_hash) in new_duplicate_slots.into_iter().zip(bank_hashes) {
            // WindowService should only send the signal once per slot
            let duplicate_state = DuplicateState::new_from_state(
                duplicate_slot,
                duplicate_confirmed_slots,
                fork_choice,
                || progress.is_dead(duplicate_slot).unwrap_or(false),
                || bank_hash,
            );
            check_slot_agrees_with_cluster(
                duplicate_slot,
                root_slot,
                blockstore,
                duplicate_slots_tracker,
                epoch_slots_frozen_slots,
                fork_choice,
                duplicate_slots_to_repair,
                ancestor_hashes_replay_update_sender,
                purge_repair_slot_counter,
                SlotStateUpdate::Duplicate(duplicate_state),
            );
        }
    }

    fn log_leader_change(
        my_pubkey: &Pubkey,
        bank_slot: Slot,
        current_leader: &mut Option<Pubkey>,
        new_leader: &Pubkey,
    ) {
        if let Some(current_leader) = current_leader.as_ref()
            && current_leader != new_leader
        {
            let msg = if Self::leader_is_me(current_leader, my_pubkey) {
                ". I am no longer the leader"
            } else if Self::leader_is_me(new_leader, my_pubkey) {
                ". I am now the leader"
            } else {
                ""
            };
            info!("LEADER CHANGE at slot: {bank_slot} leader: {new_leader}{msg}");
        }
        current_leader.replace(new_leader.to_owned());
    }

    fn check_propagation_for_start_leader(
        poh_slot: Slot,
        parent_slot: Slot,
        progress_map: &ProgressMap,
    ) -> bool {
        // Assume `NUM_CONSECUTIVE_LEADER_SLOTS` = 4. Then `skip_propagated_check`
        // below is true if `poh_slot` is within the same `NUM_CONSECUTIVE_LEADER_SLOTS`
        // set of blocks as `latest_leader_slot`.
        //
        // Example 1 (`poh_slot` directly descended from `latest_leader_slot`):
        //
        // [B B B B] [B B B latest_leader_slot] poh_slot
        //
        // Example 2:
        //
        // [B latest_leader_slot B poh_slot]
        //
        // In this example, even if there's a block `B` on another fork between
        // `poh_slot` and `parent_slot`, because they're in the same
        // `NUM_CONSECUTIVE_LEADER_SLOTS` block, we still skip the propagated
        // check because it's still within the propagation grace period.
        //
        // We've already checked in start_leader() that parent_slot hasn't been
        // dumped, so we should get it in the progress map.
        if let Some(latest_leader_slot) =
            progress_map.get_latest_leader_slot_must_exist(parent_slot)
        {
            let skip_propagated_check =
                poh_slot - latest_leader_slot < NUM_CONSECUTIVE_LEADER_SLOTS.get() as Slot;
            if skip_propagated_check {
                return true;
            }
        }

        // Note that `is_propagated(parent_slot)` doesn't necessarily check
        // propagation of `parent_slot`, it checks propagation of the latest ancestor
        // of `parent_slot` (hence the call to `get_latest_leader_slot()` in the
        // check above)
        progress_map
            .get_leader_propagation_slot_must_exist(parent_slot)
            .0
    }

    fn should_retransmit(poh_slot: Slot, last_retransmit_slot: &mut Slot) -> bool {
        if poh_slot < *last_retransmit_slot
            || poh_slot >= *last_retransmit_slot + NUM_CONSECUTIVE_LEADER_SLOTS.get() as Slot
        {
            *last_retransmit_slot = poh_slot;
            true
        } else {
            false
        }
    }

    /// Checks if it is time for us to start producing a leader block.
    /// Fails if:
    /// - Current PoH has not satisfied criteria to start my leader block
    /// - Startup verification is not complete,
    /// - Bank forks already contains a bank for this leader slot
    /// - We have not landed a vote yet and the `wait_for_vote_to_start_leader` flag is set
    /// - We have failed the propagated check
    ///
    /// Returns Some new working bank slot if created and inserted into bank forks.
    #[allow(clippy::too_many_arguments)]
    fn maybe_start_leader(
        my_pubkey: &Pubkey,
        bank_forks: &RwLock<BankForks>,
        poh_recorder: &Arc<RwLock<PohRecorder>>,
        poh_controller: &mut PohController,
        leader_schedule_cache: &Arc<LeaderScheduleCache>,
        rpc_subscriptions: Option<&RpcSubscriptions>,
        slot_status_notifier: &Option<SlotStatusNotifier>,
        progress_map: &mut ProgressMap,
        retransmit_slots_sender: &Sender<Slot>,
        skipped_slots_info: &mut SkippedSlotsInfo,
        banking_tracer: &Arc<BankingTracer>,
        has_new_vote_been_rooted: bool,
        migration_status: &MigrationStatus,
    ) -> Option<Slot> {
        assert!(!migration_status.is_alpenglow_enabled());
        // all the individual calls to poh_recorder.read() are designed to
        // increase granularity, decrease contention

        let (poh_slot, parent_slot) =
            match poh_recorder.read().unwrap().reached_leader_slot(my_pubkey) {
                PohLeaderStatus::Reached {
                    poh_slot,
                    parent_slot,
                } => (poh_slot, parent_slot),
                PohLeaderStatus::NotReached => {
                    trace!("{my_pubkey} poh_recorder hasn't reached_leader_slot");
                    return None;
                }
            };

        trace!("{my_pubkey} reached_leader_slot");

        let Some(parent) = bank_forks.read().unwrap().get(parent_slot) else {
            warn!(
                "Poh recorder parent slot {parent_slot} is missing from bank_forks. This \
                 indicates that we are in the middle of a dump and repair. Unable to start leader"
            );
            return None;
        };

        assert!(parent.is_frozen());

        if bank_forks.read().unwrap().get(poh_slot).is_some() {
            warn!("{my_pubkey} already have bank in forks at {poh_slot}?");
            return None;
        }
        trace!("{my_pubkey} poh_slot {poh_slot} parent_slot {parent_slot}");

        if let Some(next_leader) = leader_schedule_cache.slot_leader_at(poh_slot, Some(&parent)) {
            if !has_new_vote_been_rooted {
                info!("Haven't landed a vote, so skipping my leader slot");
                return None;
            }

            let next_leader_id = &next_leader.id;
            trace!("{my_pubkey} leader {next_leader_id} at poh slot: {poh_slot}");

            // I guess I missed my slot
            if !Self::leader_is_me(next_leader_id, my_pubkey) {
                return None;
            }

            datapoint_info!(
                "replay_stage-new_leader",
                ("slot", poh_slot, i64),
                ("leader", next_leader_id.to_string(), String),
            );

            if !Self::check_propagation_for_start_leader(poh_slot, parent_slot, progress_map) {
                let latest_unconfirmed_leader_slot = progress_map
                    .get_latest_leader_slot_must_exist(parent_slot)
                    .expect(
                        "In order for propagated check to fail, latest leader must exist in \
                         progress map",
                    );
                if poh_slot != skipped_slots_info.last_skipped_slot {
                    datapoint_info!(
                        "replay_stage-skip_leader_slot",
                        ("slot", poh_slot, i64),
                        ("parent_slot", parent_slot, i64),
                        (
                            "latest_unconfirmed_leader_slot",
                            latest_unconfirmed_leader_slot,
                            i64
                        )
                    );
                    progress_map.log_propagated_stats(latest_unconfirmed_leader_slot, bank_forks);
                    skipped_slots_info.last_skipped_slot = poh_slot;
                }
                if Self::should_retransmit(poh_slot, &mut skipped_slots_info.last_retransmit_slot) {
                    Self::maybe_retransmit_unpropagated_slots(
                        "replay_stage-retransmit",
                        retransmit_slots_sender,
                        progress_map,
                        latest_unconfirmed_leader_slot,
                    );
                }
                return None;
            }

            let root_slot = bank_forks.read().unwrap().root();
            datapoint_info!("replay_stage-my_leader_slot", ("slot", poh_slot, i64),);
            info!("new fork:{poh_slot} parent:{parent_slot} (leader) root:{root_slot}");

            let vote_only_bank = if migration_status.should_bank_be_vote_only(poh_slot) {
                info!("{my_pubkey}: Creating block in slot {poh_slot} in VoM");
                datapoint_info!("vote-only-bank", ("slot", poh_slot, i64));
                true
            } else {
                false
            };

            let tpu_bank = Self::new_bank_from_parent_with_notify(
                parent.clone(),
                poh_slot,
                root_slot,
                next_leader,
                rpc_subscriptions,
                slot_status_notifier,
                NewBankOptions { vote_only_bank },
            );
            // make sure parent is frozen for finalized hashes via the above
            // new()-ing of its child bank
            banking_tracer.hash_event(parent.slot(), &parent.last_blockhash(), &parent.hash());

            update_bank_forks_and_poh_recorder_for_new_tpu_bank(
                bank_forks,
                poh_controller,
                tpu_bank,
            );
            Some(poh_slot)
        } else {
            error!("{my_pubkey} No next leader found");
            None
        }
    }

    fn replay_blockstore_into_bank(
        my_shred_version: u16,
        process_active_banks_context: &ProcessActiveBanksContext,
        bank: &BankWithScheduler,
        replay_stats: &RwLock<ReplaySlotStats>,
        replay_progress: &RwLock<ConfirmationProgress>,
        finalization_cert_sender: &Sender<SigVerifiedBatch>,
    ) -> result::Result<usize, BlockstoreProcessorError> {
        let mut w_replay_stats = replay_stats.write().unwrap();
        let mut w_replay_progress = replay_progress.write().unwrap();
        let tx_count_before = w_replay_progress.num_txs;
        // All errors must lead to marking the slot as dead, otherwise,
        // the `check_slot_agrees_with_cluster()` called by `replay_active_banks()`
        // will break!
        blockstore_processor::confirm_slot(
            &process_active_banks_context.blockstore,
            bank,
            my_shred_version,
            &process_active_banks_context.replay_tx_thread_pool,
            &mut w_replay_stats,
            &mut w_replay_progress,
            false,
            process_active_banks_context
                .transaction_status_sender
                .as_ref(),
            process_active_banks_context
                .entry_notification_sender
                .as_ref(),
            Some(&process_active_banks_context.replay_vote_sender),
            Some(finalization_cert_sender),
            false,
            process_active_banks_context.log_messages_bytes_limit,
            process_active_banks_context
                .prioritization_fee_cache
                .as_deref(),
            process_active_banks_context.migration_status.as_ref(),
        )?;
        let tx_count_after = w_replay_progress.num_txs;
        let tx_count = tx_count_after - tx_count_before;
        Ok(tx_count)
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_votable_bank(
        bank: &Arc<Bank>,
        switch_fork_decision: &SwitchForkDecision,
        bank_forks: &RwLock<BankForks>,
        tower: &mut Tower,
        progress: &mut ProgressMap,
        vote_account_pubkey: &Pubkey,
        identity_keypair: &Keypair,
        authorized_voter_keypairs: &[Arc<Keypair>],
        blockstore: &Blockstore,
        leader_schedule_cache: &Arc<LeaderScheduleCache>,
        lockouts_sender: &Sender<TowerCommitmentAggregationData>,
        snapshot_controller: Option<&SnapshotController>,
        rpc_subscriptions: Option<&RpcSubscriptions>,
        block_commitment_cache: &Arc<RwLock<BlockCommitmentCache>>,
        bank_notification_sender: &Option<BankNotificationSenderConfig>,
        tracked_vote_transactions: &mut Vec<TrackedVoteTransaction>,
        has_new_vote_been_rooted: &mut bool,
        replay_timing: &mut ReplayLoopTiming,
        voting_sender: &Sender<VoteOp>,
        drop_bank_sender: &Sender<Vec<BankWithScheduler>>,
        wait_to_vote_slot: Option<Slot>,
        migration_status: &MigrationStatus,
        tbft_structs: &mut TowerBFTStructures,
    ) {
        assert!(!migration_status.is_alpenglow_enabled());
        if bank.is_empty() {
            datapoint_info!("replay_stage-voted_empty_bank", ("slot", bank.slot(), i64));
        }
        trace!("handle votable bank {}", bank.slot());
        let new_root = tower.record_bank_vote(bank).filter(|root| {
            // We do not root during the migration - post genesis rooting is handled by votor
            migration_status.should_report_commitment_or_root(*root)
        });

        if let Some(new_root) = new_root {
            let highest_super_majority_root = Some(
                block_commitment_cache
                    .read()
                    .unwrap()
                    .highest_super_majority_root(),
            );
            Self::check_and_handle_new_root(
                &identity_keypair.pubkey(),
                bank.parent_slot(),
                new_root,
                bank_forks,
                progress,
                blockstore,
                leader_schedule_cache,
                snapshot_controller,
                rpc_subscriptions,
                highest_super_majority_root,
                bank_notification_sender,
                has_new_vote_been_rooted,
                tracked_vote_transactions,
                drop_bank_sender,
                tbft_structs,
            );

            // Check if we've rooted a bank that will tell us the migration slot
            if migration_status.is_pre_feature_activation()
                && let Some(slot) = bank_forks
                    .read()
                    .unwrap()
                    .root_bank()
                    .feature_set
                    .activated_slot(&agave_feature_set::alpenglow::id())
            {
                let migration_slot = migration_status.record_feature_activation(slot);
                datapoint_info!(
                    "migration-started",
                    ("migration_slot", migration_slot as i64, i64),
                );
            }
        }

        let mut update_commitment_cache_time = Measure::start("update_commitment_cache");
        // Send (voted) bank along with the updated vote account state for this node, the vote
        // state is always newer than the one in the bank by definition, because banks can't
        // contain vote transactions which are voting on its own slot.
        //
        // It should be acceptable to aggressively use the vote for our own _local view_ of
        // commitment aggregation, although it's not guaranteed that the new vote transaction is
        // observed by other nodes at this point.
        //
        // The justification stems from the assumption of the sensible voting behavior from the
        // consensus subsystem. That's because it means there would be a slashing possibility
        // otherwise.
        //
        // This behavior isn't significant normally for mainnet-beta, because staked nodes aren't
        // servicing RPC requests. However, this eliminates artificial 1-slot delay of the
        // `finalized` confirmation if a node is materially staked and servicing RPC requests at
        // the same time for development purposes.
        let node_vote_state = (*vote_account_pubkey, tower.vote_state.clone());
        Self::update_commitment_cache(
            bank.clone(),
            bank_forks.read().unwrap().root(),
            progress.get_fork_stats(bank.slot()).unwrap().total_stake,
            node_vote_state,
            lockouts_sender,
        );
        update_commitment_cache_time.stop();
        replay_timing.update_commitment_cache_us += update_commitment_cache_time.as_us();

        Self::push_vote(
            bank,
            vote_account_pubkey,
            identity_keypair,
            authorized_voter_keypairs,
            tower,
            switch_fork_decision,
            tracked_vote_transactions,
            *has_new_vote_been_rooted,
            replay_timing,
            voting_sender,
            wait_to_vote_slot,
        );
    }

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
        if !bank.feature_set.snapshot().validator_admission_ticket {
            return;
        }

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

    fn generate_vote_tx(
        node_keypair: &Keypair,
        bank: &Bank,
        vote_account_pubkey: &Pubkey,
        authorized_voter_keypairs: &[Arc<Keypair>],
        vote: VoteTransaction,
        switch_fork_decision: &SwitchForkDecision,
        tracked_vote_transactions: &mut Vec<TrackedVoteTransaction>,
        has_new_vote_been_rooted: bool,
        wait_to_vote_slot: Option<Slot>,
    ) -> GenerateVoteTxResult {
        if authorized_voter_keypairs.is_empty() {
            return GenerateVoteTxResult::NonVoting;
        }
        if let Some(slot) = wait_to_vote_slot
            && bank.slot() < slot
        {
            return GenerateVoteTxResult::WaitToVoteSlot(slot);
        }
        let Some(vote_account) = bank.get_vote_account(vote_account_pubkey) else {
            warn!("Vote account {vote_account_pubkey} does not exist.  Unable to vote",);
            return GenerateVoteTxResult::VoteAccountNotFound(*vote_account_pubkey);
        };
        let vote_state_view = vote_account.vote_state_view();
        if vote_state_view.node_pubkey() != &node_keypair.pubkey() {
            info!(
                "Vote account node_pubkey mismatch: {} (expected: {}).  Unable to vote",
                vote_state_view.node_pubkey(),
                node_keypair.pubkey()
            );
            return GenerateVoteTxResult::HotSpare;
        }

        let Some(authorized_voter_pubkey) = vote_state_view.get_authorized_voter(bank.epoch())
        else {
            warn!(
                "Vote account {} has no authorized voter for epoch {}.  Unable to vote",
                vote_account_pubkey,
                bank.epoch()
            );
            return GenerateVoteTxResult::NoAuthorizedVoter(*vote_account_pubkey, bank.epoch());
        };

        let Some(authorized_voter_keypair) = authorized_voter_keypairs
            .iter()
            .find(|keypair| &keypair.pubkey() == authorized_voter_pubkey)
        else {
            warn!(
                "The authorized keypair {authorized_voter_pubkey} for vote account \
                 {vote_account_pubkey} is not available.  Unable to vote"
            );
            return GenerateVoteTxResult::NonVoting;
        };

        // Send our last few votes along with the new one
        // Compact the vote state update before sending
        let vote = match vote {
            VoteTransaction::VoteStateUpdate(vote_state_update) => {
                VoteTransaction::CompactVoteStateUpdate(vote_state_update)
            }
            vote => vote,
        };
        let vote_ix = switch_fork_decision
            .to_vote_instruction(
                vote,
                vote_account_pubkey,
                &authorized_voter_keypair.pubkey(),
            )
            .expect("Switch failure should not lead to voting");

        let mut vote_tx = Transaction::new_with_payer(&[vote_ix], Some(&node_keypair.pubkey()));

        let blockhash = bank.last_blockhash();
        vote_tx.partial_sign(&[node_keypair], blockhash);
        vote_tx.partial_sign(&[authorized_voter_keypair.as_ref()], blockhash);

        if !has_new_vote_been_rooted {
            let message_hash = vote_tx.message.hash();
            let recent_blockhash = vote_tx.message.recent_blockhash;
            tracked_vote_transactions.push(TrackedVoteTransaction {
                message_hash,
                transaction_blockhash: recent_blockhash,
            });
            if tracked_vote_transactions.len() > MAX_VOTE_SIGNATURES {
                tracked_vote_transactions.remove(0);
            }
        } else {
            tracked_vote_transactions.clear();
        }

        GenerateVoteTxResult::Tx(vote_tx)
    }

    /// Potentially refresh the last vote if:
    /// - We are not a hotspare or non-voting validator and we have previously attempted to vote at least once
    /// - There is a `heaviest_bank_on_same_fork` on the previously voted fork
    /// - We have previously landed a vote on this fork for a slot `latest_landed_vote_slot`
    /// - Our latest vote attempt for `last_vote_slot` has not been cleared from the progress map
    /// - `latest_landed_vote_slot` < `last_vote_slot`
    /// - The difference in block height of `heaviest_bank_on_same_fork` and `last_vote_slot`
    ///   is at least `REFRESH_VOTE_BLOCKHEIGHT` as indicated by the blockhash queue
    /// - It has been at least `MAX_VOTE_REFRESH_INTERVAL_MILLIS` ms since our last refresh
    ///
    /// If the conditions are met, we update the timestamp and blockhash of our original vote
    /// for `last_vote_slot` and resend it to the cluster
    ///
    /// Returns true if the last vote was refreshed
    #[allow(clippy::too_many_arguments)]
    fn maybe_refresh_last_vote(
        tower: &mut Tower,
        progress: &ProgressMap,
        heaviest_bank_on_same_fork: Option<Arc<Bank>>,
        vote_account_pubkey: &Pubkey,
        identity_keypair: &Keypair,
        authorized_voter_keypairs: &[Arc<Keypair>],
        tracked_vote_transactions: &mut Vec<TrackedVoteTransaction>,
        has_new_vote_been_rooted: bool,
        last_vote_refresh_time: &mut LastVoteRefreshTime,
        voting_sender: &Sender<VoteOp>,
        wait_to_vote_slot: Option<Slot>,
    ) -> bool {
        let Some(heaviest_bank_on_same_fork) = heaviest_bank_on_same_fork.as_ref() else {
            // Only refresh if blocks have been built on our last vote
            return false;
        };
        let Some(latest_landed_vote_slot) =
            progress.my_latest_landed_vote(heaviest_bank_on_same_fork.slot())
        else {
            // Need to land at least one vote in order to refresh
            return false;
        };
        let Some(last_voted_slot) = tower.last_voted_slot() else {
            // Need to have voted in order to refresh
            return false;
        };

        // If our last landed vote on this fork is greater than the vote recorded in our tower
        // this means that our tower is old AND on chain adoption has failed. Warn the operator
        // as they could be submitting slashable votes.
        if latest_landed_vote_slot > last_voted_slot
            && last_vote_refresh_time.last_print_time.elapsed().as_secs() >= 1
        {
            last_vote_refresh_time.last_print_time = Instant::now();
            warn!(
                "Last landed vote for slot {} in bank {} is greater than the current last vote \
                 for slot: {} tracked by tower. This indicates a bug in the on chain adoption \
                 logic",
                latest_landed_vote_slot,
                heaviest_bank_on_same_fork.slot(),
                last_voted_slot
            );
            datapoint_error!(
                "adoption_failure",
                ("latest_landed_vote_slot", latest_landed_vote_slot, i64),
                (
                    "heaviest_bank_on_fork",
                    heaviest_bank_on_same_fork.slot(),
                    i64
                ),
                ("last_voted_slot", last_voted_slot, i64)
            );
        }

        if latest_landed_vote_slot >= last_voted_slot {
            // Our vote or a subsequent vote landed do not refresh
            return false;
        }

        // If we are a non voting validator or have an incorrect setup preventing us from
        // generating vote txs, no need to refresh
        let last_vote_tx_blockhash = match tower.last_vote_tx_blockhash() {
            // Since the checks in vote generation are deterministic, if we were non voting or hot spare
            // on the original vote, the refresh will also fail. No reason to refresh.
            // On the fly adjustments via the cli will be picked up for the next vote.
            BlockhashStatus::NonVoting | BlockhashStatus::HotSpare => return false,
            // In this case we have not voted since restart, our setup is unclear.
            // We have a vote from our previous restart that is eligible for refresh, we must refresh.
            BlockhashStatus::Uninitialized => None,
            BlockhashStatus::Blockhash(blockhash) => Some(blockhash),
        };

        if last_vote_tx_blockhash.is_some()
            && heaviest_bank_on_same_fork
                .is_hash_valid_for_age(&last_vote_tx_blockhash.unwrap(), REFRESH_VOTE_BLOCKHEIGHT)
        {
            // Check the blockhash queue to see if enough blocks have been built on our last voted fork
            return false;
        }

        if last_vote_refresh_time
            .last_refresh_time
            .elapsed()
            .as_millis()
            < MAX_VOTE_REFRESH_INTERVAL_MILLIS as u128
        {
            // This avoids duplicate refresh in case there are multiple forks descending from our last voted fork
            // It also ensures that if the first refresh fails we will continue attempting to refresh at an interval no less
            // than MAX_VOTE_REFRESH_INTERVAL_MILLIS
            return false;
        }

        // All criteria are met, refresh the last vote using the blockhash of `heaviest_bank_on_same_fork`
        Self::refresh_last_vote(
            tower,
            heaviest_bank_on_same_fork,
            last_voted_slot,
            vote_account_pubkey,
            identity_keypair,
            authorized_voter_keypairs,
            tracked_vote_transactions,
            has_new_vote_been_rooted,
            last_vote_refresh_time,
            voting_sender,
            wait_to_vote_slot,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn refresh_last_vote(
        tower: &mut Tower,
        heaviest_bank_on_same_fork: &Bank,
        last_voted_slot: Slot,
        vote_account_pubkey: &Pubkey,
        identity_keypair: &Keypair,
        authorized_voter_keypairs: &[Arc<Keypair>],
        tracked_vote_transactions: &mut Vec<TrackedVoteTransaction>,
        has_new_vote_been_rooted: bool,
        last_vote_refresh_time: &mut LastVoteRefreshTime,
        voting_sender: &Sender<VoteOp>,
        wait_to_vote_slot: Option<Slot>,
    ) -> bool {
        // Update timestamp for refreshed vote
        tower.refresh_last_vote_timestamp(heaviest_bank_on_same_fork.slot());

        let vote_tx_result = Self::generate_vote_tx(
            identity_keypair,
            heaviest_bank_on_same_fork,
            vote_account_pubkey,
            authorized_voter_keypairs,
            tower.last_vote(),
            &SwitchForkDecision::SameFork,
            tracked_vote_transactions,
            has_new_vote_been_rooted,
            wait_to_vote_slot,
        );

        if let GenerateVoteTxResult::Tx(vote_tx) = vote_tx_result {
            let recent_blockhash = vote_tx.message.recent_blockhash;
            tower.refresh_last_vote_tx_blockhash(recent_blockhash);

            // Send the votes to the TPU and gossip for network propagation
            let hash_string = format!("{recent_blockhash}");
            datapoint_info!(
                "refresh_vote",
                ("last_voted_slot", last_voted_slot, i64),
                ("target_bank_slot", heaviest_bank_on_same_fork.slot(), i64),
                ("target_bank_hash", hash_string, String),
            );
            voting_sender
                .send(VoteOp::RefreshVote {
                    tx: vote_tx,
                    last_voted_slot,
                })
                .unwrap_or_else(|err| warn!("Error: {err:?}"));
            last_vote_refresh_time.last_refresh_time = Instant::now();
            true
        } else if vote_tx_result.is_non_voting() {
            tower.mark_last_vote_tx_blockhash_non_voting();
            false
        } else if vote_tx_result.is_hot_spare() {
            tower.mark_last_vote_tx_blockhash_hot_spare();
            false
        } else {
            false
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push_vote(
        bank: &Bank,
        vote_account_pubkey: &Pubkey,
        identity_keypair: &Keypair,
        authorized_voter_keypairs: &[Arc<Keypair>],
        tower: &mut Tower,
        switch_fork_decision: &SwitchForkDecision,
        tracked_vote_transactions: &mut Vec<TrackedVoteTransaction>,
        has_new_vote_been_rooted: bool,
        replay_timing: &mut ReplayLoopTiming,
        voting_sender: &Sender<VoteOp>,
        wait_to_vote_slot: Option<Slot>,
    ) {
        let mut generate_time = Measure::start("generate_vote");
        let vote_tx_result = Self::generate_vote_tx(
            identity_keypair,
            bank,
            vote_account_pubkey,
            authorized_voter_keypairs,
            tower.last_vote(),
            switch_fork_decision,
            tracked_vote_transactions,
            has_new_vote_been_rooted,
            wait_to_vote_slot,
        );
        generate_time.stop();
        replay_timing.generate_vote_us += generate_time.as_us();
        if let GenerateVoteTxResult::Tx(vote_tx) = vote_tx_result {
            tower.refresh_last_vote_tx_blockhash(vote_tx.message.recent_blockhash);

            let saved_tower = SavedTower::new(tower, identity_keypair).unwrap_or_else(|err| {
                error!("Unable to create saved tower: {err:?}");
                std::process::exit(1);
            });

            let tower_slots = tower.tower_slots();
            voting_sender
                .send(VoteOp::PushVote {
                    tx: vote_tx,
                    tower_slots,
                    saved_tower: SavedTowerVersions::from(saved_tower),
                })
                .unwrap_or_else(|err| warn!("Error: {err:?}"));
        } else if vote_tx_result.is_non_voting() {
            tower.mark_last_vote_tx_blockhash_non_voting();
        }
    }

    fn update_commitment_cache(
        bank: Arc<Bank>,
        root: Slot,
        total_stake: Stake,
        node_vote_state: (Pubkey, TowerVoteState),
        lockouts_sender: &Sender<TowerCommitmentAggregationData>,
    ) {
        if let Err(e) = lockouts_sender.send(TowerCommitmentAggregationData::new(
            bank,
            root,
            total_stake,
            node_vote_state,
        )) {
            trace!("lockouts_sender failed: {e:?}");
        }
    }

    fn reset_poh_recorder(
        my_pubkey: &Pubkey,
        blockstore: &Blockstore,
        bank: Arc<Bank>,
        poh_controller: &mut PohController,
        leader_schedule_cache: &LeaderScheduleCache,
    ) {
        let slot = bank.slot();
        let tick_height = bank.tick_height();

        let next_leader_slot = leader_schedule_cache.next_leader_slot(
            my_pubkey,
            slot,
            &bank,
            Some(blockstore),
            GRACE_TICKS_FACTOR * MAX_GRACE_SLOTS,
        );

        if poh_controller.reset(bank, next_leader_slot).is_err() {
            warn!("Failed to reset poh, poh service is disconnected");
            return;
        }

        let next_leader_msg = if let Some(next_leader_slot) = next_leader_slot {
            format!("My next leader slot is {}", next_leader_slot.0)
        } else {
            "I am not in the leader schedule yet".to_owned()
        };
        info!(
            "{my_pubkey} reset PoH to tick {tick_height} (within slot {slot}). {next_leader_msg}",
        );
    }

    // Verifies this fork has not been marked dead and that the slot is rooted.
    // After verification, grabs or creates a fork progress entry, extracts
    // tracking structures, and packages these with the correlated bank so they
    // can be updated while replaying.
    fn prepare_active_bank_for_replay(
        process_active_banks_context: &ProcessActiveBanksContext,
        progress: &mut ProgressMap,
        async_verification_freelist: &mut Vec<AsyncVerificationProgress>,
        my_pubkey: &Pubkey,
        vote_account: &Pubkey,
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
        let prev_leader_slot = progress.get_bank_prev_leader_slot(&bank);
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
                my_pubkey,
                vote_account,
                prev_leader_slot,
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
        my_pubkey: &Pubkey,
        vote_account: &Pubkey,
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
                    my_pubkey,
                    vote_account,
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
        my_pubkey: &Pubkey,
        finalization_cert_sender: &Sender<SigVerifiedBatch>,
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

        if Self::leader_is_me(bank.leader_id(), my_pubkey) {
            return (replay_result, None);
        }

        // Check if the child block's chained merkle root chains to the parent's block id.
        // It's important that we do this here (after we have a bank) rather than failing
        // in generate_new_bank_forks, as we need a bank to mark as dead in order to kick off
        // ancestor hashes service / duplicate block repair.
        match check_chained_block_id(
            &process_active_banks_context.blockstore,
            &bank,
            process_active_banks_context.migration_status.as_ref(),
        ) {
            ChainedBlockIdCheck::Inactive | ChainedBlockIdCheck::Pass => (),
            ChainedBlockIdCheck::Unavailable => {
                // Missing shred 0, can't replay anyway
                return (replay_result, None);
            }
            ChainedBlockIdCheck::Mismatch => {
                // Mismatch, return a replay error so `process_replay_results`
                // marks the bank hard-dead and publishes normal dead-slot
                // notifications.
                replay_result.replay_result =
                    Some(Err(BlockstoreProcessorError::ChainedBlockIdFailure(
                        bank.slot(),
                        bank.parent_slot(),
                    )));
                return (replay_result, None);
            }
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

    /// Live replay must not execute this validator's own leader banks from
    /// blockstore. Those banks are driven by BankingStage/PoH while the node is
    /// live; already-recorded own slots are replayed by startup ledger replay
    /// before ReplayStage starts.
    fn leader_is_me(slot_leader: &Pubkey, my_pubkey: &Pubkey) -> bool {
        slot_leader == my_pubkey
    }

    fn replay_active_banks(
        my_shred_version: u16,
        process_active_banks_context: &ProcessActiveBanksContext,
        bank_replay_result_trackers: Vec<BankReplayResultTracker>,
        replay_timing: &mut ReplayLoopTiming,
        my_pubkey: &Pubkey,
        finalization_cert_sender: &Sender<SigVerifiedBatch>,
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
                                        my_pubkey,
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
                        my_pubkey,
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
        process_active_banks_context: &ProcessActiveBanksContext,
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

        // Send this whether the block was valid or not. It is only used to
        // release buffered votes, if any.
        let _ =
            process_active_banks_context
                .replay_vote_sender
                .send(ReplayVoteMessage::BankComplete {
                    replay_bank_id: bank.bank_id(),
                    replay_slot: bank.slot(),
                });

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
        latest_validator_votes_for_frozen_banks: &mut LatestValidatorVotesForFrozenBanks,
        duplicate_slots_to_repair: &mut DuplicateSlotsToRepair,
        purge_repair_slot_counter: &mut PurgeRepairSlotCounter,
        mut tbft_structs: Option<&mut TowerBFTStructures>,
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
                            duplicate_slots_to_repair,
                            purge_repair_slot_counter,
                            tbft_structs.as_deref_mut(),
                        );
                        continue;
                    }
                    Err(err) => {
                        mark_replay_dead_slot(
                            bank,
                            err,
                            progress,
                            &mut process_active_banks_context.dead_slot_context(
                                duplicate_slots_to_repair,
                                purge_repair_slot_counter,
                                tbft_structs.as_deref_mut(),
                            ),
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
                    process_active_banks_context,
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
                            &mut process_active_banks_context.dead_slot_context(
                                duplicate_slots_to_repair,
                                purge_repair_slot_counter,
                                tbft_structs.as_deref_mut(),
                            ),
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
                    .get_block_id(bank.slot(), &process_active_banks_context.migration_status)
                    .expect("Blockstore operations must succeed");
                debug_assert!(block_id.is_some() || is_leader_block);
                if block_id.is_some() {
                    bank.set_block_id(block_id);
                }

                // Freeze the bank before sending to any auxiliary threads that may expect to be
                // operating on a frozen bank.
                // Also if we are not the leader, ensure that our computed hash matches the hash in
                // the block footer.
                let verify_result = if process_active_banks_context
                    .migration_status
                    .should_allow_block_markers(bank.slot())
                    && !Self::leader_is_me(bank.leader_id(), my_pubkey)
                {
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

                    if let Err(err) = bank_hash_details::write_bank_hash_details_file(bank) {
                        warn!("Unable to write bank hash details file: {err}");
                    }

                    mark_replay_dead_slot(
                        bank,
                        &BlockstoreProcessorError::BankHashMismatch(
                            bank_slot,
                            expected_hash,
                            computed_hash,
                        ),
                        progress,
                        &mut process_active_banks_context.dead_slot_context(
                            duplicate_slots_to_repair,
                            purge_repair_slot_counter,
                            tbft_structs.as_deref_mut(),
                        ),
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
                if process_active_banks_context
                    .migration_status
                    .should_publish_epoch_slots(bank_slot)
                {
                    let _ = process_active_banks_context
                        .cluster_slots_update_sender
                        .send(vec![bank_slot]);
                }

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
                bank_progress.fork_stats.bank_hash = Some(bank.hash());
                if let Some(TowerBFTStructures {
                    heaviest_subtree_fork_choice,
                    duplicate_slots_tracker,
                    duplicate_confirmed_slots,
                    epoch_slots_frozen_slots,
                    ..
                }) = &mut tbft_structs
                {
                    // Needs to be updated before `check_slot_agrees_with_cluster()` so that
                    // any updates in `check_slot_agrees_with_cluster()` on fork choice take
                    // effect
                    heaviest_subtree_fork_choice.add_new_leaf_slot(
                        (bank.slot(), bank.hash()),
                        Some((bank.parent_slot(), bank.parent_hash())),
                    );
                    heaviest_subtree_fork_choice.maybe_print_state();
                    let bank_frozen_state = BankFrozenState::new_from_state(
                        bank.slot(),
                        bank.hash(),
                        duplicate_slots_tracker,
                        duplicate_confirmed_slots,
                        heaviest_subtree_fork_choice,
                        epoch_slots_frozen_slots,
                    );
                    check_slot_agrees_with_cluster(
                        bank.slot(),
                        bank_forks.read().unwrap().root(),
                        &process_active_banks_context.blockstore,
                        duplicate_slots_tracker,
                        epoch_slots_frozen_slots,
                        heaviest_subtree_fork_choice,
                        duplicate_slots_to_repair,
                        &process_active_banks_context.ancestor_hashes_replay_update_sender,
                        purge_repair_slot_counter,
                        SlotStateUpdate::BankFrozen(bank_frozen_state),
                    );
                    // If we previously marked this slot as duplicate in blockstore, let the state machine know
                    if !duplicate_slots_tracker.contains(&bank.slot())
                        && process_active_banks_context
                            .blockstore
                            .get_duplicate_slot(bank.slot())
                            .is_some()
                    {
                        let duplicate_state = DuplicateState::new_from_state(
                            bank.slot(),
                            duplicate_confirmed_slots,
                            heaviest_subtree_fork_choice,
                            || false,
                            || Some(bank.hash()),
                        );
                        check_slot_agrees_with_cluster(
                            bank.slot(),
                            bank_forks.read().unwrap().root(),
                            &process_active_banks_context.blockstore,
                            duplicate_slots_tracker,
                            epoch_slots_frozen_slots,
                            heaviest_subtree_fork_choice,
                            duplicate_slots_to_repair,
                            &process_active_banks_context.ancestor_hashes_replay_update_sender,
                            purge_repair_slot_counter,
                            SlotStateUpdate::Duplicate(duplicate_state),
                        );
                    }
                }

                // For leader banks:
                // 1) Replay finishes before shredding, broadcast_stage will take care of
                //      notifying votor
                // 2) Shredding finishes before replay, we notify here
                //
                // For non leader banks (2) is always true, so notify here
                if process_active_banks_context
                    .migration_status
                    .should_send_votor_event(bank.slot())
                    && bank.block_id().is_some()
                {
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

                let bank_hash = bank.hash();
                if let Some(new_frozen_voters) = tbft_structs.as_mut().and_then(|tbft| {
                    tbft.unfrozen_gossip_verified_vote_hashes
                        .remove_slot_hash(bank.slot(), &bank_hash)
                }) {
                    for pubkey in new_frozen_voters {
                        latest_validator_votes_for_frozen_banks.check_add_vote(
                            pubkey,
                            bank.slot(),
                            Some(bank_hash),
                            false,
                        );
                    }
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
        latest_validator_votes_for_frozen_banks: &mut LatestValidatorVotesForFrozenBanks,
        duplicate_slots_to_repair: &mut DuplicateSlotsToRepair,
        purge_repair_slot_counter: &mut PurgeRepairSlotCounter,
        tbft_structs: Option<&mut TowerBFTStructures>,
        my_pubkey: &Pubkey,
        vote_account: &Pubkey,
        replay_timing: &mut ReplayLoopTiming,
        finalization_cert_sender: &Sender<SigVerifiedBatch>,
    ) -> Vec<Slot> /* completed slots */ {
        let bank_replay_result_trackers = Self::prepare_active_banks_for_replay(
            process_active_banks_context,
            progress,
            async_verification_freelist,
            my_pubkey,
            vote_account,
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
            my_pubkey,
            finalization_cert_sender,
        );

        // Process replay results.
        Self::process_replay_results(
            process_active_banks_context,
            progress,
            async_verification_freelist,
            latest_validator_votes_for_frozen_banks,
            duplicate_slots_to_repair,
            purge_repair_slot_counter,
            tbft_structs,
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
    ) {
        let next_slot = bank.slot().saturating_add(1);

        let is_next_leader = leader_schedule_cache
            .slot_leader_at(next_slot, Some(bank))
            .is_some_and(|leader| Self::leader_is_me(&leader.id, my_pubkey));

        if !is_next_leader {
            return;
        }

        if next_slot != first_of_consecutive_leader_slots(next_slot) {
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
                block_id,
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
    pub fn compute_bank_stats(
        my_vote_pubkey: &Pubkey,
        ancestors: &HashMap<u64, HashSet<u64>>,
        frozen_banks: &mut [Arc<Bank>],
        tower: &mut Tower,
        progress: &mut ProgressMap,
        vote_tracker: &VoteTracker,
        cluster_slots: &ClusterSlots,
        bank_forks: &RwLock<BankForks>,
        heaviest_subtree_fork_choice: &mut HeaviestSubtreeForkChoice,
        latest_validator_votes_for_frozen_banks: &mut LatestValidatorVotesForFrozenBanks,
        vote_slots: &mut HashSet<Slot, ahash::RandomState>,
        migration_status: &MigrationStatus,
    ) -> Vec<Slot> {
        frozen_banks.sort_by_key(|bank| bank.slot());
        let mut new_stats = vec![];
        for bank in frozen_banks.iter() {
            let bank_slot = bank.slot();
            // Only time progress map should be missing a bank slot
            // is if this node was the leader for this slot as those banks
            // are not replayed in replay_active_banks()
            {
                let is_computed = progress
                    .get_fork_stats_mut(bank_slot)
                    .expect("All frozen banks must exist in the Progress map")
                    .computed;

                if !is_computed {
                    // Check if our tower is behind, if so adopt the on chain tower from this Bank
                    Self::adopt_on_chain_tower_if_behind(
                        my_vote_pubkey,
                        ancestors,
                        frozen_banks,
                        tower,
                        progress,
                        bank,
                        bank_forks,
                    );

                    let root_slot = bank_forks.read().unwrap().root();
                    let computed_bank_state = Tower::collect_vote_lockouts(
                        my_vote_pubkey,
                        bank_slot,
                        bank.parent_slot(),
                        root_slot,
                        &bank.vote_accounts(),
                        ancestors,
                        |slot| progress.get_hash(slot),
                        latest_validator_votes_for_frozen_banks,
                        vote_slots,
                    );
                    // Notify any listeners of the votes found in this newly computed
                    // bank
                    heaviest_subtree_fork_choice.compute_bank_stats(
                        bank,
                        tower,
                        latest_validator_votes_for_frozen_banks,
                    );
                    let ComputedBankState {
                        voted_stakes,
                        total_stake,
                        fork_stake,
                        lockout_intervals,
                        my_latest_landed_vote,
                        parent_is_super_oc,
                        ..
                    } = computed_bank_state;

                    if parent_is_super_oc
                        && migration_status.qualifies_for_genesis_discovery(bank_slot)
                    {
                        let migration_slot =
                            migration_status.migration_slot().expect("In migration");
                        // We have a block whose ancestor qualifies to be the genesis block.
                        let genesis_slot = ancestors
                            .get(&bank_slot)
                            .expect(
                                "Ancestors must exist, as this cannot be slot 0, and no roots are \
                                 made during migration",
                            )
                            .iter()
                            .filter(|slot| **slot < migration_slot)
                            .max()
                            .copied()
                            .expect("Genesis slot must exist, no rooting past migration slot");
                        let genesis_bank = bank_forks
                            .read()
                            .unwrap()
                            .get(genesis_slot)
                            .expect("Genesis bank must exist, no rooting past migration slot");

                        let genesis_block = Block {
                            slot: genesis_slot,
                            block_id: genesis_bank.block_id().expect(
                                "It is impossible for block id to not be known at this point, as \
                                 a descendant of this block has reached super oc status",
                            ),
                        };
                        migration_status.set_genesis_block(genesis_block);
                    }

                    let stats = progress
                        .get_fork_stats_mut(bank_slot)
                        .expect("All frozen banks must exist in the Progress map");
                    stats.fork_stake = fork_stake;
                    stats.total_stake = total_stake;
                    stats.voted_stakes = voted_stakes;
                    stats.lockout_intervals = lockout_intervals;
                    stats.block_height = bank.block_height();
                    stats.my_latest_landed_vote = my_latest_landed_vote;
                    stats.computed = true;
                    new_stats.push(bank_slot);
                    datapoint_info!(
                        "bank_weight",
                        ("slot", bank_slot, i64),
                        ("fork_stake", stats.fork_stake, i64),
                        ("fork_weight", stats.fork_weight(), f64),
                    );

                    info!(
                        "{} slot_weight: {} {:.1}% {}",
                        my_vote_pubkey,
                        bank_slot,
                        100.0 * stats.fork_weight(), // percentage fork_stake in total_stake
                        bank.parent().map(|b| b.slot()).unwrap_or(0)
                    );
                }
            }

            Self::update_propagation_status(
                progress,
                bank_slot,
                bank_forks,
                vote_tracker,
                cluster_slots,
            );

            Self::cache_tower_stats(progress, tower, bank_slot, ancestors);
        }

        // `vote_slots` steady state is 32 entries (tower height), but may grow if a node is experiencing a partition.
        // As such, this should generally be a noop, but should avoid maintaining unnecessary capacity in
        // exceptional cases.
        vote_slots.shrink_to(MAX_LOCKOUT_HISTORY + 1);

        new_stats
    }

    fn adopt_on_chain_tower_if_behind(
        my_vote_pubkey: &Pubkey,
        ancestors: &HashMap<Slot, HashSet<Slot>>,
        frozen_banks: &[Arc<Bank>],
        tower: &mut Tower,
        progress: &mut ProgressMap,
        bank: &Arc<Bank>,
        bank_forks: &RwLock<BankForks>,
    ) {
        let Some(vote_account) = bank.get_vote_account(my_vote_pubkey) else {
            return;
        };
        let mut bank_vote_state = TowerVoteState::from(vote_account.vote_state_view());
        if bank_vote_state.last_voted_slot() <= tower.vote_state.last_voted_slot() {
            return;
        }
        info!(
            "Frozen bank vote state slot {:?} is newer than our local vote state slot {:?}, \
             adopting the bank vote state as our own. Bank votes: {:?}, root: {:?}, Local votes: \
             {:?}, root: {:?}",
            bank_vote_state.last_voted_slot(),
            tower.vote_state.last_voted_slot(),
            bank_vote_state.votes,
            bank_vote_state.root_slot,
            tower.vote_state.votes,
            tower.vote_state.root_slot
        );

        if let Some(local_root) = tower.vote_state.root_slot
            && bank_vote_state
                .root_slot
                .map(|bank_root| local_root > bank_root)
                .unwrap_or(true)
        {
            // If the local root is larger than this on chain vote state
            // root (possible due to supermajority roots being set on
            // startup), then we need to adjust the tower
            bank_vote_state.root_slot = Some(local_root);
            bank_vote_state
                .votes
                .retain(|lockout| lockout.slot() > local_root);
            info!(
                "Local root is larger than on chain root, overwrote bank root {:?} and updated \
                 votes {:?}",
                bank_vote_state.root_slot, bank_vote_state.votes
            );

            if let Some(first_vote) = bank_vote_state.votes.front() {
                assert!(
                    ancestors
                        .get(&first_vote.slot())
                        .expect(
                            "Ancestors map must contain an entry for all slots on this fork \
                             greater than `local_root` and less than `bank_slot`"
                        )
                        .contains(&local_root)
                );
            }
        }

        // adopt the bank vote state
        tower.vote_state = bank_vote_state;

        let last_voted_slot = tower.vote_state.last_voted_slot().unwrap_or(
            // If our local root is higher than the highest slot in `bank_vote_state` due to
            // supermajority roots, then it's expected that the vote state will be empty.
            // In this case we use the root as our last vote. This root cannot be None, because
            // `tower.vote_state.last_voted_slot()` is None only if `tower.vote_state.root_slot`
            // is Some.
            tower
                .vote_state
                .root_slot
                .expect("root_slot cannot be None here"),
        );
        // This is safe because `last_voted_slot` is now equal to
        // `bank_vote_state.last_voted_slot()` or `local_root`.
        // Since this vote state is contained in `bank`, which we have frozen,
        // we must have frozen all slots contained in `bank_vote_state`,
        // and by definition we must have frozen `local_root`.
        //
        // If `bank` is a duplicate, since we are able to replay it successfully, any slots
        // in its vote state must also be part of the duplicate fork, and thus present in our
        // progress map.
        //
        // Finally if both `bank` and `bank_vote_state.last_voted_slot()` are duplicate,
        // we must have the compatible versions of both duplicates in order to replay `bank`
        // successfully, so we are once again guaranteed that `bank_vote_state.last_voted_slot()`
        // is present in bank forks and progress map.
        let block_id = {
            // The block_id here will only be relevant if we need to refresh this last vote.
            let bank = bank_forks
                .read()
                .unwrap()
                .get(last_voted_slot)
                .expect("Last voted slot that we are adopting must exist in bank forks");
            // Here we don't have to check if this is our leader bank, as since we are adopting this bank,
            // that means that it was created from a different instance (hot spare setup or a previous restart),
            // and thus we must have replayed and set the block_id from the shreds.
            // Note: since the new shred format is not rolled out everywhere, we have to provide a default
            bank.block_id().unwrap_or_default()
        };
        tower.update_last_vote_from_vote_state(
            progress
                .get_hash(last_voted_slot)
                .expect("Must exist for us to have frozen descendant"),
            block_id,
        );
        // Since we are updating our tower we need to update associated caches for previously computed
        // slots as well.
        for slot in frozen_banks.iter().map(|b| b.slot()) {
            if !progress
                .get_fork_stats(slot)
                .expect("All frozen banks must exist in fork stats")
                .computed
            {
                continue;
            }
            Self::cache_tower_stats(progress, tower, slot, ancestors);
        }
    }

    fn cache_tower_stats(
        progress: &mut ProgressMap,
        tower: &Tower,
        slot: Slot,
        ancestors: &HashMap<u64, HashSet<u64>>,
    ) {
        let stats = progress
            .get_fork_stats_mut(slot)
            .expect("All frozen banks must exist in the Progress map");

        stats.vote_threshold =
            tower.check_vote_stake_thresholds(slot, &stats.voted_stakes, stats.total_stake);
        stats.is_locked_out = tower.is_locked_out(
            slot,
            ancestors
                .get(&slot)
                .expect("Ancestors map should contain slot for is_locked_out() check"),
        );
        stats.has_voted = tower.has_voted(slot);
        stats.is_recent = tower.is_recent(slot);
    }

    fn update_propagation_status(
        progress: &mut ProgressMap,
        slot: Slot,
        bank_forks: &RwLock<BankForks>,
        vote_tracker: &VoteTracker,
        cluster_slots: &ClusterSlots,
    ) {
        // We would only reach here if the bank is in bank_forks, so it
        // isn't dumped and should exist in progress map.
        // If propagation has already been confirmed, return
        if progress.get_leader_propagation_slot_must_exist(slot).0 {
            return;
        }

        // Otherwise we have to check the votes for confirmation
        let propagated_stats = progress
            .get_propagated_stats_mut(slot)
            .unwrap_or_else(|| panic!("slot={slot} must exist in ProgressMap"));

        if propagated_stats.slot_vote_tracker.is_none() {
            propagated_stats.slot_vote_tracker = vote_tracker.get_slot_vote_tracker(slot);
        }
        let slot_vote_tracker = propagated_stats.slot_vote_tracker.clone();

        if propagated_stats.cluster_slot_pubkeys.is_none() {
            propagated_stats.cluster_slot_pubkeys = cluster_slots.lookup(slot);
        }
        let cluster_slot_pubkeys = propagated_stats.cluster_slot_pubkeys.clone();

        let newly_voted_pubkeys = slot_vote_tracker
            .as_ref()
            .and_then(|slot_vote_tracker| {
                slot_vote_tracker.write().unwrap().get_voted_slot_updates()
            })
            .unwrap_or_default();

        let cluster_slot_pubkeys = cluster_slot_pubkeys
            .map(|v| v.keys().cloned().collect())
            .unwrap_or_default();

        Self::update_fork_propagated_threshold_from_votes(
            progress,
            newly_voted_pubkeys,
            cluster_slot_pubkeys,
            slot,
            &bank_forks.read().unwrap(),
        );
    }

    fn update_fork_propagated_threshold_from_votes(
        progress: &mut ProgressMap,
        mut newly_voted_pubkeys: Vec<Pubkey>,
        mut cluster_slot_pubkeys: Vec<Pubkey>,
        fork_tip: Slot,
        bank_forks: &BankForks,
    ) {
        // We would only reach here if the bank is in bank_forks, so it
        // isn't dumped and should exist in progress map.
        let mut current_leader_slot = progress.get_latest_leader_slot_must_exist(fork_tip);
        let mut did_newly_reach_threshold = false;
        let root = bank_forks.root();
        loop {
            // These cases mean confirmation of propagation on any earlier
            // leader blocks must have been reached
            if current_leader_slot.is_none() || current_leader_slot.unwrap() < root {
                break;
            }

            let leader_propagated_stats = progress
                .get_propagated_stats_mut(current_leader_slot.unwrap())
                .expect("current_leader_slot >= root, so must exist in the progress map");

            // If a descendant has reached propagation threshold, then
            // all its ancestor banks have also reached propagation
            // threshold as well (Validators can't have voted for a
            // descendant without also getting the ancestor block)
            if leader_propagated_stats.is_propagated || {
                // If there's no new validators to record, and there's no
                // newly achieved threshold, then there's no further
                // information to propagate backwards to past leader blocks
                newly_voted_pubkeys.is_empty()
                    && cluster_slot_pubkeys.is_empty()
                    && !did_newly_reach_threshold
            } {
                break;
            }

            // We only iterate through the list of leader slots by traversing
            // the linked list of 'prev_leader_slot`'s outlined in the
            // `progress` map
            assert!(leader_propagated_stats.is_leader_slot);
            let leader_bank = bank_forks
                .get(current_leader_slot.unwrap())
                .expect("Entry in progress map must exist in BankForks")
                .clone();

            did_newly_reach_threshold = Self::update_slot_propagated_threshold_from_votes(
                &mut newly_voted_pubkeys,
                &mut cluster_slot_pubkeys,
                &leader_bank,
                leader_propagated_stats,
                did_newly_reach_threshold,
            ) || did_newly_reach_threshold;

            // Now jump to process the previous leader slot
            current_leader_slot = leader_propagated_stats.prev_leader_slot;
        }
    }

    fn update_slot_propagated_threshold_from_votes(
        newly_voted_pubkeys: &mut Vec<Pubkey>,
        cluster_slot_pubkeys: &mut Vec<Pubkey>,
        leader_bank: &Bank,
        leader_propagated_stats: &mut PropagatedStats,
        did_child_reach_threshold: bool,
    ) -> bool {
        // Track whether this slot newly confirm propagation
        // throughout the network (switched from is_propagated == false
        // to is_propagated == true)
        let mut did_newly_reach_threshold = false;

        // If a child of this slot confirmed propagation, then
        // we can return early as this implies this slot must also
        // be propagated
        if did_child_reach_threshold {
            if !leader_propagated_stats.is_propagated {
                leader_propagated_stats.is_propagated = true;
                return true;
            } else {
                return false;
            }
        }

        if leader_propagated_stats.is_propagated {
            return false;
        }

        // Remove the vote/node pubkeys that we already know voted for this
        // slot. These vote accounts/validator identities are safe to drop
        // because they don't to be ported back any further because earlier
        // parents must have:
        // 1) Also recorded these pubkeys already, or
        // 2) Already reached the propagation threshold, in which case
        //    they no longer need to track the set of propagated validators
        newly_voted_pubkeys.retain(|vote_pubkey| {
            let exists = leader_propagated_stats
                .propagated_validators
                .contains(vote_pubkey);
            leader_propagated_stats.add_vote_pubkey(
                *vote_pubkey,
                leader_bank.epoch_vote_account_stake(vote_pubkey),
            );
            !exists
        });

        cluster_slot_pubkeys.retain(|node_pubkey| {
            let exists = leader_propagated_stats
                .propagated_node_ids
                .contains(node_pubkey);
            leader_propagated_stats.add_node_pubkey(node_pubkey, leader_bank);
            !exists
        });

        if leader_propagated_stats.total_epoch_stake == 0
            || leader_propagated_stats.propagated_validators_stake as f64
                / leader_propagated_stats.total_epoch_stake as f64
                > SUPERMINORITY_THRESHOLD
        {
            leader_propagated_stats.is_propagated = true;
            did_newly_reach_threshold = true
        }

        did_newly_reach_threshold
    }

    #[allow(clippy::too_many_arguments)]
    fn mark_slots_duplicate_confirmed(
        confirmed_slots: &[(Slot, Hash)],
        blockstore: &Blockstore,
        bank_forks: &RwLock<BankForks>,
        progress: &mut ProgressMap,
        duplicate_slots_tracker: &mut DuplicateSlotsTracker,
        fork_choice: &mut HeaviestSubtreeForkChoice,
        epoch_slots_frozen_slots: &mut EpochSlotsFrozenSlots,
        duplicate_slots_to_repair: &mut DuplicateSlotsToRepair,
        ancestor_hashes_replay_update_sender: &AncestorHashesReplayUpdateSender,
        purge_repair_slot_counter: &mut PurgeRepairSlotCounter,
        duplicate_confirmed_slots: &mut DuplicateConfirmedSlots,
    ) {
        let root_slot = bank_forks.read().unwrap().root();
        for (slot, frozen_hash) in confirmed_slots.iter() {
            assert!(*frozen_hash != Hash::default());

            if *slot <= root_slot {
                continue;
            }

            progress.set_duplicate_confirmed_hash(*slot, *frozen_hash);
            if let Some(prev_hash) = duplicate_confirmed_slots.insert(*slot, *frozen_hash) {
                assert_eq!(
                    prev_hash, *frozen_hash,
                    "Additional duplicate confirmed notification for slot {slot} with a different \
                     hash"
                );
                // Already processed this signal
                continue;
            }

            let duplicate_confirmed_state = DuplicateConfirmedState::new_from_state(
                *frozen_hash,
                || false,
                || Some(*frozen_hash),
            );
            check_slot_agrees_with_cluster(
                *slot,
                root_slot,
                blockstore,
                duplicate_slots_tracker,
                epoch_slots_frozen_slots,
                fork_choice,
                duplicate_slots_to_repair,
                ancestor_hashes_replay_update_sender,
                purge_repair_slot_counter,
                SlotStateUpdate::DuplicateConfirmed(duplicate_confirmed_state),
            );
        }
    }

    fn tower_duplicate_confirmed_forks(
        tower: &Tower,
        voted_stakes: &VotedStakes,
        total_stake: Stake,
        progress: &ProgressMap,
        bank_forks: &RwLock<BankForks>,
    ) -> Vec<(Slot, Hash)> {
        let mut duplicate_confirmed_forks = vec![];
        for (slot, prog) in progress.iter() {
            if prog.fork_stats.duplicate_confirmed_hash.is_some() {
                continue;
            }
            let Some(bank) = bank_forks.read().unwrap().get(*slot) else {
                continue;
            };
            let duration = prog
                .replay_stats
                .read()
                .unwrap()
                .started
                .elapsed()
                .as_millis();
            if !bank.is_frozen() {
                continue;
            }
            if tower.is_slot_duplicate_confirmed(*slot, voted_stakes, total_stake) {
                info!(
                    "validator fork duplicate confirmed {} {}ms",
                    *slot, duration
                );
                datapoint_info!(
                    "validator-duplicate-confirmation",
                    ("duration_ms", duration, i64)
                );
                duplicate_confirmed_forks.push((*slot, bank.hash()));
            } else {
                debug!(
                    "validator fork not confirmed {} {}ms {:?}",
                    *slot,
                    duration,
                    voted_stakes.get(slot)
                );
            }
        }
        duplicate_confirmed_forks
    }

    #[allow(clippy::too_many_arguments)]
    /// A wrapper around `root_utils::check_and_handle_new_root` which:
    /// - calls into `root_utils::set_bank_forks_root`
    /// - Executes `set_progress_and_tower_bft_root` to cleanup tower bft structs and the progress map
    fn check_and_handle_new_root(
        my_pubkey: &Pubkey,
        parent_slot: Slot,
        new_root: Slot,
        bank_forks: &RwLock<BankForks>,
        progress: &mut ProgressMap,
        blockstore: &Blockstore,
        leader_schedule_cache: &Arc<LeaderScheduleCache>,
        snapshot_controller: Option<&SnapshotController>,
        rpc_subscriptions: Option<&RpcSubscriptions>,
        highest_super_majority_root: Option<Slot>,
        bank_notification_sender: &Option<BankNotificationSenderConfig>,
        has_new_vote_been_rooted: &mut bool,
        tracked_vote_transactions: &mut Vec<TrackedVoteTransaction>,
        drop_bank_sender: &Sender<Vec<BankWithScheduler>>,
        tbft_structs: &mut TowerBFTStructures,
    ) {
        root_utils::check_and_handle_new_root(
            parent_slot,
            new_root,
            snapshot_controller,
            highest_super_majority_root,
            bank_notification_sender,
            drop_bank_sender,
            blockstore,
            leader_schedule_cache,
            bank_forks,
            rpc_subscriptions,
            my_pubkey,
            move |bank_forks| {
                Self::set_progress_and_tower_bft_root(
                    new_root,
                    bank_forks,
                    progress,
                    has_new_vote_been_rooted,
                    tracked_vote_transactions,
                    tbft_structs,
                )
            },
        )
    }

    // To avoid code duplication and keep compatibility with alpenglow, we add this
    // extra callback in the rooting path. This happens immediately after setting the bank forks root
    fn set_progress_and_tower_bft_root(
        new_root: Slot,
        bank_forks: &BankForks,
        progress: &mut ProgressMap,
        has_new_vote_been_rooted: &mut bool,
        tracked_vote_transactions: &mut Vec<TrackedVoteTransaction>,
        tbft_structs: &mut TowerBFTStructures,
    ) {
        let new_root_bank = &bank_forks[new_root];
        if !*has_new_vote_been_rooted {
            for TrackedVoteTransaction {
                message_hash,
                transaction_blockhash,
            } in tracked_vote_transactions.iter()
            {
                if new_root_bank
                    .get_committed_transaction_status_and_slot(message_hash, transaction_blockhash)
                    .is_some()
                {
                    *has_new_vote_been_rooted = true;
                    break;
                }
            }
            if *has_new_vote_been_rooted {
                std::mem::take(tracked_vote_transactions);
            }
        }

        progress.handle_new_root(bank_forks);
        let TowerBFTStructures {
            heaviest_subtree_fork_choice,
            duplicate_slots_tracker,
            duplicate_confirmed_slots,
            unfrozen_gossip_verified_vote_hashes,
            epoch_slots_frozen_slots,
            ..
        } = tbft_structs;
        heaviest_subtree_fork_choice.set_tree_root((new_root, bank_forks.root_bank().hash()));
        *duplicate_slots_tracker = duplicate_slots_tracker.split_off(&new_root);
        // duplicate_slots_tracker now only contains entries >= `new_root`

        *duplicate_confirmed_slots = duplicate_confirmed_slots.split_off(&new_root);
        // gossip_confirmed_slots now only contains entries >= `new_root`

        unfrozen_gossip_verified_vote_hashes.set_root(new_root);
        *epoch_slots_frozen_slots = epoch_slots_frozen_slots.split_off(&new_root);
        // epoch_slots_frozen_slots now only contains entries >= `new_root`
    }

    #[allow(clippy::too_many_arguments)]
    /// A wrapper around `root_utils::set_bank_forks_root` which additionally:
    /// - Executes `set_progress_and_tower_bft_root` to cleanup tower bft structs and the progress map
    pub fn handle_new_root(
        new_root: Slot,
        bank_forks: &RwLock<BankForks>,
        progress: &mut ProgressMap,
        snapshot_controller: Option<&SnapshotController>,
        highest_super_majority_root: Option<Slot>,
        has_new_vote_been_rooted: &mut bool,
        tracked_vote_transactions: &mut Vec<TrackedVoteTransaction>,
        drop_bank_sender: &Sender<Vec<BankWithScheduler>>,
        tbft_structs: &mut TowerBFTStructures,
    ) {
        root_utils::set_bank_forks_root(
            new_root,
            bank_forks,
            snapshot_controller,
            highest_super_majority_root,
            drop_bank_sender,
            move |bank_forks| {
                Self::set_progress_and_tower_bft_root(
                    new_root,
                    bank_forks,
                    progress,
                    has_new_vote_been_rooted,
                    tracked_vote_transactions,
                    tbft_structs,
                )
            },
        );
    }

    /// Process any commands from the bank forks controller
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
        let SetRootCommand {
            parent_slot,
            new_root,
            highest_super_majority_root,
        } = command;
        root_utils::check_and_handle_new_root(
            parent_slot,
            new_root,
            context.snapshot_controller.as_deref(),
            highest_super_majority_root,
            &context.bank_notification_sender,
            &context.drop_bank_sender,
            &context.blockstore,
            &context.leader_schedule_cache,
            &context.bank_forks,
            context.rpc_subscriptions.as_deref(),
            my_pubkey,
            |bank_forks| progress.handle_new_root(bank_forks),
        );
    }

    /// Process a bank forks command
    fn process_bank_forks_command(
        command: BankForksCommand,
        context: &ProcessBankForksContext,
        progress: &mut ProgressMap,
        async_verification_freelist: &mut Vec<AsyncVerificationProgress>,
    ) {
        debug_assert!(
            context
                .bank_forks
                .read()
                .unwrap()
                .migration_status()
                .is_alpenglow_enabled()
        );
        match command {
            BankForksCommand::InsertBank {
                bank,
                response_sender,
            } => {
                // Check that the bank is still valid to be inserted
                let bank = {
                    let mut bank_forks = context.bank_forks.write().unwrap();
                    if bank_forks.get(bank.slot()).is_none()
                        && bank_forks.get(bank.parent_slot()).is_some()
                    {
                        Some(bank_forks.insert(*bank))
                    } else {
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
                Self::clear_banks(
                    &BTreeSet::from([slot]),
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
            migration_status,
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

                // Live ReplayStage should never create banks for our own
                // leader slots. BCL/PoH own live block production, and startup
                // replay handles any already-recorded own blocks after restart.
                if Self::leader_is_me(&leader.id, my_pubkey) {
                    trace!("skipping replay bank creation for own leader slot {child_slot}");
                    continue;
                }

                let replay_offset = match child_bank_replay_start(
                    blockstore,
                    parent_bank,
                    parent_slot,
                    child_slot,
                    migration_status,
                ) {
                    ChildBankReplayStart::FromStart => None,
                    ChildBankReplayStart::FromUpdateParent(num_shreds) => Some(num_shreds),
                    ChildBankReplayStart::Defer => continue,
                };

                info!("new fork:{child_slot} parent:{parent_slot} root:{root}",);
                // Migration period banks are VoM
                let options = NewBankOptions {
                    vote_only_bank: migration_status.should_bank_be_vote_only(child_slot),
                };
                if options.vote_only_bank {
                    info!("Replaying block in slot {child_slot} in VoM");
                }
                let child_bank = Self::new_bank_from_parent_with_notify(
                    parent_bank.clone(),
                    child_slot,
                    root,
                    leader,
                    rpc_subscriptions,
                    slot_status_notifier,
                    options,
                );
                blockstore_processor::set_alpenglow_ticks(&child_bank, migration_status);

                let empty: Vec<Pubkey> = vec![];
                Self::update_fork_propagated_threshold_from_votes(
                    progress,
                    empty,
                    vec![leader.id],
                    parent_bank.slot(),
                    &bank_forks.read().unwrap(),
                );
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
                    let prev_leader_slot = progress.get_bank_prev_leader_slot(&bank);
                    let fork_progress = ForkProgress::new(
                        parent_bank.last_blockhash(),
                        prev_leader_slot,
                        None,
                        0,
                        0,
                        None,
                    );
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
        new_bank_options: NewBankOptions,
    ) -> Bank {
        if let Some(rpc_subscriptions) = rpc_subscriptions {
            rpc_subscriptions.notify_slot(slot, parent.slot(), root_slot);
        }
        let parent_slot = parent.slot();
        let bank = Bank::new_from_parent_with_options(parent, leader, slot, new_bank_options);
        if let Some(slot_status_notifier) = slot_status_notifier {
            slot_status_notifier.read().unwrap().notify_created_bank(
                slot,
                parent_slot,
                bank.bank_id(),
            );
        }
        bank
    }

    fn log_heaviest_fork_failures(
        heaviest_fork_failures: &Vec<HeaviestForkFailures>,
        bank_forks: &RwLock<BankForks>,
        tower: &Tower,
        progress: &ProgressMap,
        ancestors: &HashMap<Slot, HashSet<Slot>>,
        heaviest_bank: &Arc<Bank>,
        last_threshold_failure_slot: &mut Slot,
    ) {
        info!(
            "Couldn't vote on heaviest fork: {:?}, heaviest_fork_failures: {:?}",
            heaviest_bank.slot(),
            heaviest_fork_failures
        );

        for failure in heaviest_fork_failures {
            match failure {
                HeaviestForkFailures::NoPropagatedConfirmation(slot, ..) => {
                    // If failure is NoPropagatedConfirmation, then inside select_vote_and_reset_forks
                    // we already confirmed it's in progress map, we should see it in progress map
                    // here because we don't have dump and repair in between.
                    if let Some(latest_leader_slot) =
                        progress.get_latest_leader_slot_must_exist(*slot)
                    {
                        progress.log_propagated_stats(latest_leader_slot, bank_forks);
                    }
                }
                &HeaviestForkFailures::FailedThreshold(
                    slot,
                    depth,
                    observed_stake,
                    total_stake,
                ) => {
                    if slot > *last_threshold_failure_slot {
                        *last_threshold_failure_slot = slot;
                        let in_partition = if let Some(last_voted_slot) = tower.last_voted_slot() {
                            Self::is_partition_detected(
                                ancestors,
                                last_voted_slot,
                                heaviest_bank.slot(),
                            )
                        } else {
                            false
                        };
                        datapoint_info!(
                            "replay_stage-threshold-failure",
                            ("slot", slot as i64, i64),
                            ("depth", depth as i64, i64),
                            ("observed_stake", observed_stake as i64, i64),
                            ("total_stake", total_stake as i64, i64),
                            ("in_partition", in_partition, bool),
                        );
                    }
                }
                // These are already logged in the partition info
                HeaviestForkFailures::LockedOut(_)
                | HeaviestForkFailures::FailedSwitchThreshold(_, _, _) => (),
            }
        }
    }

    pub fn join(self) -> thread::Result<()> {
        self.t_replay.join().map(|_| ())
    }
}

#[cfg(test)]
pub(crate) mod tests;
