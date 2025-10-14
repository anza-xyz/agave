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
        consensus_pool_service::ConsensusPoolService,
        event::{LeaderWindowInfo, VotorEventReceiver, VotorEventSender},
        event_handler::EventHandler,
        timer_manager::TimerManager,
        vote_history::VoteHistory,
        vote_history_storage::VoteHistoryStorage,
        voting_service::BLSOp,
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
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Condvar, Mutex, RwLock,
        },
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

    // Receivers
    pub event_receiver: VotorEventReceiver,
    pub consensus_message_receiver: Receiver<ConsensusMessage>,
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
}

impl Votor {
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
}
