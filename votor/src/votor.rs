//! The entrypoint into votor the module responsible for voting, rooting, and notifying
//! the core to create a new block.
//! ```text
//!
//!                                Votor
//!   ┌────────────────────────────────────────────────────────────────────────────┐
//!   │                                                                            │
//!   │                                                     Push Certificate       │
//!   │        ┌───────────────────────────────────────────────────────────────────│────────┐
//!   │        │                   Parent Ready                                    │        │
//!   │        │                   Standstill                                      │        │
//!   │        │                   Finalized                                       │        │
//!   │        │                   Block Notarized                                 │        │
//!   │        │         ┌─────────Safe To Notar/Skip───┐       Push               │        │
//!   │        │         │         Produce Window       │       Vote               │        │
//!   │        │         │                              │ ┌────────────────────────│──────┐ │
//!   │        │         │                              │ │                        │ ┌────▼─▼───────┐
//!   │        │         │                              │ │                        │ │Voting Service│
//!   │        │         │                              │ │                        │ └──────────────┘
//!   │        │         │                              │ │                        │
//!   │   ┌────┼─────────┼───────────────┐              │ │    Switch Bank         │
//!   │   │                              │              │ │      Block             │ ┌────────────────────┐
//!   │   │   Consensus Pool Service     │              │ │  ┌─────────────────────│─┼ Replay / Broadcast │
//!   │   │                              │              │ │  │                     │ └────────────────────┘
//!   │   │ ┌──────────────────────────┐ │              │ │  │                     │
//!   │   │ │                          │ │              │ │  │                     │
//!   │   │ │     Consensus Pool       │ │              │ │  │                     │
//!   │   │ │ ┌────────────────────┐   │ │         ┌────▼─┼──▼───────┐   Start     │
//!   │   │ │ │Parent ready tracker│   │ │ Vote    │                 │ Leader window ┌──────────────────────┐
//!   │   │ │ └────────────────────┘   │ ◄─────────┼  Event Handler  ┼─────────────│─►  Block creation loop │
//!   │   │ └──────────────────────────┘ │         │                 │             │ └──────────────────────┘
//!   │   │                              │         └─▲───────────┬───┘             │
//!   │   └──────────────────────────────┘           │           │ \               │
//!   │                                     Timeout  │           │  \  RepairEvent │ ┌───────────────────────┐
//!   │                                              │           │   \─────────────│─► BlockID Repair Service│
//!   │                                              │           │                 │ └───────────────────────┘
//!   │                                              │           │ Set Timeouts    │
//!   │                                              │           │                 │
//!   │                          ┌───────────────────┴┐     ┌────▼───────────────┐ │
//!   │                          │                    │     │                    │ │
//!   │                          │ Timer Service      ┼─────┼ timer Manager      │ │
//!   │                          │                    │     │                    │ │
//!   │                          └────────────────────┘     └────────────────────┘ │
//!   └────────────────────────────────────────────────────────────────────────────┘
//! ```
use {
    crate::{
        commitment::CommitmentAggregationData,
        consensus_metrics::ConsensusMetrics,
        consensus_pool_service::{
            ConsensusPoolContext, ConsensusPoolService, staked_status::StakedStatus,
        },
        event::{
            LatestSwitchRequest, LeaderWindowInfo, RepairEventSender, VotorEventReceiver,
            VotorEventSender,
        },
        event_handler::{EventHandler, EventHandlerContext},
        root_utils::RootContext,
        slot_clock::SharedAlpenglowSlotClock,
        timer_manager::TimerManager,
        vote_history::VoteHistory,
        vote_history_storage::VoteHistoryStorage,
        voting_service::BLSOp,
        voting_utils::VotingContext,
    },
    agave_bls_sigverify::{generated_cert_types::GeneratedCertTypes, rewards::RewardInput},
    agave_votor_messages::{
        certificate::Certificate,
        consensus_message::{Block, VoteMessage},
        metric_types::{ConsensusMetricsEventReceiver, ConsensusMetricsEventSender},
        sig_verified_messages::SigVerifiedBatch,
    },
    crossbeam_channel::{Receiver, Sender},
    parking_lot::RwLock as PlRwLock,
    smallvec::SmallVec,
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_keypair::Keypair,
    solana_ledger::{blockstore::Blockstore, leader_schedule_cache::LeaderScheduleCache},
    solana_pubkey::Pubkey,
    solana_rpc::optimistically_confirmed_bank_tracker::BankNotificationSenderConfig,
    solana_runtime::{
        bank_forks::BankForks, bank_forks_controller::BankForksController,
        validated_block_finalization::ValidatedBlockFinalizationCert,
    },
    solana_streamer::evicting_sender::EvictingSender,
    solana_validator_exit::Exit,
    std::{
        collections::HashMap,
        sync::{Arc, RwLock, atomic::AtomicBool},
        thread::{self, JoinHandle},
        time::Duration,
    },
};

/// Brings the whole validator down when a votor thread stops.
pub(crate) struct ExitOnDrop {
    validator_exit: Arc<RwLock<Exit>>,
}

impl ExitOnDrop {
    pub(crate) fn new(validator_exit: Arc<RwLock<Exit>>) -> Self {
        Self { validator_exit }
    }
}

impl Drop for ExitOnDrop {
    fn drop(&mut self) {
        if let Ok(mut validator_exit) = self.validator_exit.write() {
            validator_exit.exit();
        }
    }
}

/// Inputs to Votor
pub struct VotorConfig {
    pub exit: Arc<AtomicBool>,
    pub validator_exit: Arc<RwLock<Exit>>,
    // Validator config
    pub vote_account: Pubkey,
    pub wait_to_vote_slot: Option<Slot>,
    pub vote_history: VoteHistory,
    pub vote_history_storage: Arc<dyn VoteHistoryStorage>,
    pub generated_cert_types: Arc<GeneratedCertTypes>,

    // Shared state
    pub authorized_voter_keypairs: Arc<RwLock<Vec<Arc<Keypair>>>>,
    pub blockstore: Arc<Blockstore>,
    pub bank_forks: Arc<RwLock<BankForks>>,
    pub cluster_info: Arc<ClusterInfo>,
    pub leader_schedule_cache: Arc<LeaderScheduleCache>,
    pub alpenglow_slot_clock: SharedAlpenglowSlotClock,
    pub consensus_metrics_sender: ConsensusMetricsEventSender,
    pub highest_finalized: Arc<RwLock<Option<ValidatedBlockFinalizationCert>>>,
    pub bank_forks_controller: Arc<dyn BankForksController>,

    // Senders / Notifiers
    pub bls_sender: Sender<BLSOp>,
    pub commitment_sender: Sender<CommitmentAggregationData>,
    pub bank_notification_sender: Option<BankNotificationSenderConfig>,
    pub leader_window_info_sender: Sender<LeaderWindowInfo>,
    pub highest_parent_ready: Arc<RwLock<(Slot, Block)>>,
    pub event_sender: VotorEventSender,
    pub own_vote_sender: EvictingSender<VoteMessage>,
    pub own_reward_aggregates_sender: Sender<RewardInput>,
    pub repair_event_sender: RepairEventSender,
    pub latest_switch_request: LatestSwitchRequest,

    // Receivers
    pub event_receiver: VotorEventReceiver,
    pub consensus_message_receiver: Receiver<SigVerifiedBatch>,
    pub own_votes_receiver: Receiver<VoteMessage>,
    pub footer_certs_receiver: Receiver<SmallVec<[Certificate; 2]>>,
    pub consensus_metrics_receiver: ConsensusMetricsEventReceiver,
}

/// Context shared with block creation, replay, gossip, banking stage etc
pub(crate) struct SharedContext {
    pub(crate) blockstore: Arc<Blockstore>,
    pub(crate) bank_forks: Arc<RwLock<BankForks>>,
    pub(crate) cluster_info: Arc<ClusterInfo>,
    pub(crate) alpenglow_slot_clock: SharedAlpenglowSlotClock,
    pub(crate) leader_window_info_sender: Sender<LeaderWindowInfo>,
    pub(crate) highest_parent_ready: Arc<RwLock<(Slot, Block)>>,
    pub(crate) vote_history_storage: Arc<dyn VoteHistoryStorage>,
    pub(crate) repair_event_sender: RepairEventSender,
    pub(crate) latest_switch_request: LatestSwitchRequest,
}

pub struct Votor {
    event_handler: EventHandler,
    consensus_pool_service: ConsensusPoolService,
    timer_manager: Arc<PlRwLock<TimerManager>>,
    metrics: JoinHandle<()>,
}

impl Votor {
    pub fn new(config: VotorConfig) -> Self {
        let VotorConfig {
            exit,
            validator_exit,
            vote_account,
            wait_to_vote_slot,
            vote_history,
            vote_history_storage,
            authorized_voter_keypairs,
            blockstore,
            bank_forks,
            cluster_info,
            leader_schedule_cache,
            alpenglow_slot_clock,
            bls_sender,
            commitment_sender,
            bank_notification_sender,
            leader_window_info_sender,
            highest_parent_ready,
            event_sender,
            own_vote_sender,
            own_reward_aggregates_sender,
            repair_event_sender,
            latest_switch_request,
            event_receiver,
            consensus_message_receiver,
            consensus_metrics_sender,
            consensus_metrics_receiver,
            generated_cert_types,
            highest_finalized,
            bank_forks_controller,
            own_votes_receiver,
            footer_certs_receiver,
        } = config;

        let migration_status = bank_forks.read().unwrap().migration_status();
        let identity_keypair = cluster_info.keypair();
        let vote_history_highest_parent_ready = vote_history.highest_parent_ready();

        // Get the sharable root bank
        let sharable_banks = bank_forks.read().unwrap().sharable_banks();

        let shared_context = SharedContext {
            blockstore: blockstore.clone(),
            bank_forks,
            cluster_info: cluster_info.clone(),
            alpenglow_slot_clock,
            highest_parent_ready,
            leader_window_info_sender,
            vote_history_storage: vote_history_storage.clone(),
            repair_event_sender: repair_event_sender.clone(),
            latest_switch_request,
        };

        let voting_context = VotingContext {
            cluster_info: cluster_info.clone(),
            leader_schedule: leader_schedule_cache.clone(),
            vote_history,
            vote_account_pubkey: vote_account,
            identity_keypair,
            authorized_voter_keypairs,
            vote_history_storage,
            derived_bls_keypairs: HashMap::new(),
            own_vote_sender,
            own_reward_sender: own_reward_aggregates_sender,
            bls_sender: bls_sender.clone(),
            commitment_sender: commitment_sender.clone(),
            wait_to_vote_slot,
            sharable_banks: sharable_banks.clone(),
            consensus_metrics_sender,
        };

        let root_context = RootContext {
            bank_notification_sender,
            bank_forks_controller,
        };

        let timer_manager = Arc::new(PlRwLock::new(TimerManager::new(
            cluster_info.clone(),
            event_sender.clone(),
            exit.clone(),
            validator_exit.clone(),
            migration_status.clone(),
        )));

        let event_handler_context = EventHandlerContext {
            exit: exit.clone(),
            validator_exit: validator_exit.clone(),
            migration_status: migration_status.clone(),
            event_receiver,
            timer_manager: Arc::clone(&timer_manager),
            shared_context,
            voting_context,
            root_context,
        };

        let root_bank = sharable_banks.root();
        let epoch_schedule = root_bank.epoch_schedule().clone();

        let consensus_pool_context = ConsensusPoolContext {
            exit: exit.clone(),
            validator_exit,
            migration_status,
            generated_cert_types,
            cluster_info: cluster_info.clone(),
            blockstore,
            sharable_banks: sharable_banks.clone(),
            leader_schedule_cache: leader_schedule_cache.clone(),
            vote_history_highest_parent_ready,
            consensus_message_receiver,
            footer_certs_receiver,
            own_votes_receiver,
            bls_sender,
            event_sender,
            repair_event_sender,
            highest_finalized,
            staked_status: StakedStatus::new(&root_bank, &cluster_info),
        };

        let metrics = ConsensusMetrics::start_metrics_loop(
            epoch_schedule,
            consensus_metrics_receiver,
            exit.clone(),
        );
        let event_handler = EventHandler::new(event_handler_context);
        let consensus_pool_service = ConsensusPoolService::new(consensus_pool_context);

        Self {
            event_handler,
            consensus_pool_service,
            timer_manager,
            metrics,
        }
    }

    pub fn join(self) -> thread::Result<()> {
        self.consensus_pool_service.join()?;

        // Loop till we manage to unwrap the Arc and then we can join.
        let mut timer_manager = self.timer_manager;
        loop {
            match Arc::try_unwrap(timer_manager) {
                Ok(manager) => {
                    manager.into_inner().join();
                    break;
                }
                Err(m) => {
                    timer_manager = m;
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }
        self.metrics.join()?;
        self.event_handler.join()
    }
}
