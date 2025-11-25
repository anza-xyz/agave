//! ```text
//! The entrypoint into votor the module responsible for voting, rooting, and notifying
//! the core to create a new block.
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
//!   │   ┌────┼─────────┼───────────────┐              │ │                        │
//!   │   │                              │              │ │      Block             │ ┌────────────────────┐
//!   │   │    Consensus Pool Service    │              │ │  ┌─────────────────────│─┼ Replay / Broadcast │
//!   │   │                              │              │ │  │                     │ └────────────────────┘
//!   │   │ ┌──────────────────────────┐ │              │ │  │                     │
//!   │   │ │                          │ │              │ │  │                     │
//!   │   │ │     Consensus Pool       │ │              │ │  │                     │
//!   │   │ │ ┌────────────────────┐   │ │         ┌────▼─┼──▼───────┐   Start     │
//!   │   │ │ │Parent ready tracker│   │ │ Vote    │                 │ Leader window ┌──────────────────────┐
//!   │   │ │ └────────────────────┘   │ ◄─────────┼  Event Handler  ┼─────────────│─►  Block creation loop │
//!   │   │ └──────────────────────────┘ │         │                 │             │ └──────────────────────┘
//!   │   │                              │         └─▲───────────┬───┘             │
//!   │   └──────────────────────────────┘           │           │                 │
//!   │                                     Timeout  │           │                 │
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
        common::DELTA_STANDSTILL,
        consensus_metrics::{
            ConsensusMetrics, ConsensusMetricsEventReceiver, ConsensusMetricsEventSender,
        },
        consensus_pool_service::{ConsensusPoolContext, ConsensusPoolService},
        event::{LeaderWindowInfo, VotorEventReceiver, VotorEventSender},
        event_handler::{EventHandler, EventHandlerContext},
        root_utils::RootContext,
        timer_manager::TimerManager,
        vote_history::VoteHistory,
        vote_history_storage::VoteHistoryStorage,
        voting_service::BLSOp,
        voting_utils::VotingContext,
    },
    agave_votor_messages::consensus_message::ConsensusMessage,
    crossbeam_channel::{Receiver, Sender},
    parking_lot::RwLock as PlRwLock,
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_ledger::{blockstore::Blockstore, leader_schedule_cache::LeaderScheduleCache},
    solana_pubkey::Pubkey,
    solana_rpc::{
        optimistically_confirmed_bank_tracker::BankNotificationSenderConfig,
        rpc_subscriptions::RpcSubscriptions,
    },
    solana_runtime::{
        bank_forks::BankForks, installed_scheduler_pool::BankWithScheduler,
        snapshot_controller::SnapshotController,
    },
    std::{
        collections::HashMap,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Condvar, Mutex, RwLock,
        },
        thread::{self, JoinHandle},
        time::Duration,
    },
};

/// Communication with the block creation loop to notify leader window
#[derive(Default)]
pub struct LeaderWindowNotifier {
    pub window_info: Mutex<Option<LeaderWindowInfo>>,
    pub window_notification: Condvar,
    pub highest_parent_ready: RwLock<(Slot, (Slot, Hash))>,
}

/// Inputs to Votor
pub struct VotorConfig {
    pub exit: Arc<AtomicBool>,
    // Validator config
    pub vote_account: Pubkey,
    pub wait_to_vote_slot: Option<Slot>,
    pub wait_for_vote_to_start_leader: bool,
    pub vote_history: VoteHistory,
    pub vote_history_storage: Arc<dyn VoteHistoryStorage>,

    // Shared state
    pub authorized_voter_keypairs: Arc<RwLock<Vec<Arc<Keypair>>>>,
    pub blockstore: Arc<Blockstore>,
    pub bank_forks: Arc<RwLock<BankForks>>,
    pub cluster_info: Arc<ClusterInfo>,
    pub leader_schedule_cache: Arc<LeaderScheduleCache>,
    pub rpc_subscriptions: Option<Arc<RpcSubscriptions>>,

    // Senders / Notifiers
    pub snapshot_controller: Option<Arc<SnapshotController>>,
    pub bls_sender: Sender<BLSOp>,
    pub commitment_sender: Sender<CommitmentAggregationData>,
    pub drop_bank_sender: Sender<Vec<BankWithScheduler>>,
    pub bank_notification_sender: Option<BankNotificationSenderConfig>,
    pub leader_window_notifier: Arc<LeaderWindowNotifier>,
    pub event_sender: VotorEventSender,
    pub own_vote_sender: Sender<ConsensusMessage>,
    pub consensus_metrics_sender: ConsensusMetricsEventSender,

    // Receivers
    pub event_receiver: VotorEventReceiver,
    pub consensus_message_receiver: Receiver<ConsensusMessage>,
    pub consensus_metrics_receiver: ConsensusMetricsEventReceiver,
}

/// Context shared with block creation, replay, gossip, banking stage etc
pub(crate) struct SharedContext {
    pub(crate) blockstore: Arc<Blockstore>,
    pub(crate) bank_forks: Arc<RwLock<BankForks>>,
    pub(crate) cluster_info: Arc<ClusterInfo>,
    pub(crate) rpc_subscriptions: Option<Arc<RpcSubscriptions>>,
    pub(crate) leader_window_notifier: Arc<LeaderWindowNotifier>,
    pub(crate) vote_history_storage: Arc<dyn VoteHistoryStorage>,
}

pub struct Votor {
    // TODO: Just a placeholder for how migration could look like,
    // will fix once we finish the strategy
    start: Arc<(Mutex<bool>, Condvar)>,

    event_handler: EventHandler,
    consensus_pool_service: ConsensusPoolService,
    timer_manager: Arc<PlRwLock<TimerManager>>,
    consensus_metrics_handle: JoinHandle<()>,
}

impl Votor {
    pub fn new(config: VotorConfig) -> Self {
        let VotorConfig {
            exit,
            vote_account,
            wait_to_vote_slot,
            wait_for_vote_to_start_leader,
            vote_history,
            vote_history_storage,
            authorized_voter_keypairs,
            blockstore,
            bank_forks,
            cluster_info,
            leader_schedule_cache,
            rpc_subscriptions,
            snapshot_controller,
            bls_sender,
            commitment_sender,
            drop_bank_sender,
            bank_notification_sender,
            leader_window_notifier,
            event_sender,
            event_receiver,
            own_vote_sender,
            consensus_message_receiver,
            consensus_metrics_sender,
            consensus_metrics_receiver,
        } = config;

        let start = Arc::new((Mutex::new(false), Condvar::new()));

        let identity_keypair = cluster_info.keypair().clone();
        let has_new_vote_been_rooted = !wait_for_vote_to_start_leader;

        // Get the sharable root bank
        let sharable_banks = bank_forks.read().unwrap().sharable_banks();

        let shared_context = SharedContext {
            blockstore: blockstore.clone(),
            bank_forks: bank_forks.clone(),
            cluster_info: cluster_info.clone(),
            rpc_subscriptions,
            leader_window_notifier,
            vote_history_storage,
        };

        let voting_context = VotingContext {
            vote_history,
            vote_account_pubkey: vote_account,
            identity_keypair: identity_keypair.clone(),
            authorized_voter_keypairs,
            derived_bls_keypairs: HashMap::new(),
            has_new_vote_been_rooted,
            own_vote_sender,
            bls_sender: bls_sender.clone(),
            commitment_sender: commitment_sender.clone(),
            wait_to_vote_slot,
            sharable_banks: sharable_banks.clone(),
            consensus_metrics_sender: consensus_metrics_sender.clone(),
        };

        let root_context = RootContext {
            leader_schedule_cache: leader_schedule_cache.clone(),
            snapshot_controller,
            bank_notification_sender,
            drop_bank_sender,
        };

        let timer_manager = Arc::new(PlRwLock::new(TimerManager::new(
            event_sender.clone(),
            exit.clone(),
        )));

        let event_handler_context = EventHandlerContext {
            exit: exit.clone(),
            start: start.clone(),
            event_receiver,
            timer_manager: Arc::clone(&timer_manager),
            shared_context,
            voting_context,
            root_context,
        };

        let root_epoch = sharable_banks.root().epoch();

        let consensus_pool_context = ConsensusPoolContext {
            exit: exit.clone(),
            start: start.clone(),
            cluster_info: cluster_info.clone(),
            my_vote_pubkey: vote_account,
            blockstore,
            sharable_banks,
            leader_schedule_cache,
            consensus_message_receiver,
            bls_sender,
            event_sender,
            commitment_sender,
            delta_standstill: DELTA_STANDSTILL,
        };

        let consensus_metrics_handle = ConsensusMetrics::start_metrics_loop(
            root_epoch,
            consensus_metrics_receiver,
            exit.clone(),
        );
        let event_handler = EventHandler::new(event_handler_context);
        let consensus_pool_service = ConsensusPoolService::new(consensus_pool_context);

        Self {
            start,
            event_handler,
            consensus_pool_service,
            timer_manager,
            consensus_metrics_handle,
        }
    }

    pub fn start_migration(&self) {
        // TODO: evaluate once we have actual migration logic
        let (lock, cvar) = &*self.start;
        let mut started = lock.lock().unwrap();
        *started = true;
        cvar.notify_all();
    }

    pub(crate) fn wait_for_migration_or_exit(
        exit: &AtomicBool,
        (lock, cvar): &(Mutex<bool>, Condvar),
    ) {
        let mut started = lock.lock().unwrap();
        while !*started {
            if exit.load(Ordering::Relaxed) {
                return;
            }
            // Add timeout to check for exit flag. Check infrequent enough to
            // not hit performance while frequent enough that validator exit
            // isn't delayed a lot.
            (started, _) = cvar.wait_timeout(started, Duration::from_secs(1)).unwrap();
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
        self.event_handler.join()?;
        self.consensus_metrics_handle.join()
    }
}
