use {
    agave_votor_messages::consensus_message::Block,
    crossbeam_channel::{Receiver, Sender},
    solana_clock::Slot,
    solana_runtime::bank::Bank,
    std::{sync::Arc, time::Instant},
};

#[derive(Debug, Clone)]
pub struct CompletedBlock {
    pub slot: Slot,
    // TODO: once we have the async execution changes this can be (block_id, parent_block_id) instead
    pub bank: Arc<Bank>,
}

/// Context for the block creation loop to start a leader window
#[derive(Copy, Clone, Debug)]
pub struct LeaderWindowInfo {
    pub start_slot: Slot,
    pub end_slot: Slot,
    pub parent_block: Block,
    pub skip_timer: Instant,
}

pub type VotorEventSender = Sender<VotorEvent>;
pub type VotorEventReceiver = Receiver<VotorEvent>;

/// Events that trigger actions in Votor
/// TODO: remove bank hash once we update votes
#[derive(Debug, Clone)]
pub enum VotorEvent {
    /// A block has completed replay and is ready for voting
    Block(CompletedBlock),

    /// The block has received a notarization certificate
    BlockNotarized(Block),

    /// Received the first shred for the slot.
    FirstShred(Slot),

    /// The pool has marked the given block as a ready parent for `slot`
    ParentReady { slot: Slot, parent_block: Block },

    //// Timeout to early detect that a honest that has crashed and
    /// if the leader window should be skipped.
    TimeoutCrashedLeader(Slot),

    /// Timeout to inspect whether the remaining leader window should be skipped.
    Timeout(Slot),

    /// The given block has reached the safe to notar status
    SafeToNotar(Block),

    /// The given slot has reached the safe to skip status
    SafeToSkip(Slot),

    /// We are the leader for this window and have reached the parent ready status
    /// Produce the window
    ProduceWindow(LeaderWindowInfo),

    /// The block has received a slow or fast finalization certificate and is eligible for rooting
    /// The second bool indicates whether the block is a fast finalization
    Finalized(Block, bool),

    /// We have not observed a finalization and reached the standstill timeout
    /// The slot is the highest finalized slot
    Standstill(Slot),

    /// The identity keypair has changed due to an operator calling set-identity
    SetIdentity,
}

impl VotorEvent {
    /// Ignore old events
    pub(crate) fn should_ignore(&self, root: Slot) -> bool {
        match self {
            VotorEvent::Block(completed_block) => completed_block.slot <= root,
            VotorEvent::Timeout(s)
            | VotorEvent::SafeToSkip(s)
            | VotorEvent::TimeoutCrashedLeader(s)
            | VotorEvent::FirstShred(s)
            | VotorEvent::SafeToNotar((s, _))
            | VotorEvent::Finalized((s, _), _)
            | VotorEvent::BlockNotarized((s, _))
            | VotorEvent::ParentReady {
                slot: s,
                parent_block: _,
            } => s <= &root,
            VotorEvent::ProduceWindow(_) => false,
            VotorEvent::Standstill(_) => false,
            VotorEvent::SetIdentity => false,
        }
    }
}
