//! Handles incoming VotorEvents to take action or
//! notify block creation loop
use {
    crate::{
        event::VotorEventReceiver,
        root_utils::RootContext,
        timer_manager::TimerManager,
        vote_history::VoteHistoryError,
        voting_utils::{VoteError, VotingContext},
        votor::SharedContext,
    },
    agave_votor_messages::consensus_message::Block,
    crossbeam_channel::{RecvError, SendError},
    parking_lot::RwLock,
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    std::{
        collections::{BTreeMap, BTreeSet},
        sync::{atomic::AtomicBool, Arc, Condvar, Mutex},
        thread::JoinHandle,
    },
    thiserror::Error,
};

/// Banks that have completed replay, but are yet to be voted on
/// in the form of (block, parent block)
pub(crate) type PendingBlocks = BTreeMap<Slot, Vec<(Block, Block)>>;

/// Inputs for the event handler thread
pub(crate) struct EventHandlerContext {
    pub(crate) exit: Arc<AtomicBool>,
    pub(crate) start: Arc<(Mutex<bool>, Condvar)>,

    pub(crate) event_receiver: VotorEventReceiver,
    pub(crate) timer_manager: Arc<RwLock<TimerManager>>,

    // Contexts
    pub(crate) shared_context: SharedContext,
    pub(crate) voting_context: VotingContext,
    pub(crate) root_context: RootContext,
}

#[derive(Debug, Error)]
enum EventLoopError {
    #[error("Receiver is disconnected")]
    ReceiverDisconnected(#[from] RecvError),

    #[error("Sender is disconnected")]
    SenderDisconnected(#[from] SendError<()>),

    #[error("Error generating and inserting vote")]
    VotingError(#[from] VoteError),

    #[error("Set identity error")]
    SetIdentityError(#[from] VoteHistoryError),
}

pub(crate) struct EventHandler {
    t_event_handler: JoinHandle<()>,
}

struct LocalContext {
    pub(crate) my_pubkey: Pubkey,
    pub(crate) pending_blocks: PendingBlocks,
    pub(crate) finalized_blocks: BTreeSet<Block>,
    pub(crate) received_shred: BTreeSet<Slot>,
}
