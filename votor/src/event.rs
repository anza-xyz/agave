use {
    agave_votor_messages::consensus_message::Block,
    crossbeam_channel::{Receiver, Sender},
    solana_clock::Slot,
    solana_runtime::bank::Bank,
    std::{
        sync::{Arc, Mutex},
        time::Instant,
    },
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
    pub block_timer: Instant,
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

    /// The block has received a notar-fallback certificate
    BlockNotarFallback(Block),

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
            | VotorEvent::SafeToNotar(Block {
                slot: s,
                block_id: _,
            })
            | VotorEvent::Finalized(
                Block {
                    slot: s,
                    block_id: _,
                },
                _,
            )
            | VotorEvent::BlockNotarized(Block {
                slot: s,
                block_id: _,
            })
            | VotorEvent::BlockNotarFallback(Block {
                slot: s,
                block_id: _,
            })
            | VotorEvent::ParentReady {
                slot: s,
                parent_block: _,
            } => s <= &root,
            VotorEvent::Standstill(s) => s < &root,
            VotorEvent::ProduceWindow(info) => info.start_slot <= root,
            VotorEvent::SetIdentity => false,
        }
    }
}

pub type RepairEventSender = Sender<RepairEvent>;
pub type RepairEventReceiver = Receiver<RepairEvent>;

/// Event sent by votor to the block id repair service for informed repair
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum RepairEvent {
    /// We require that this block be fetched. This can happen for the following reasons:
    /// - The block has received a NotarizeFallback certificate or stronger
    /// - An intrawindow block has reached the SafeToNotar threshold, however we need
    ///   to check that the parent has reached notarize-fallback requiring us to fetch this block
    FetchBlock { block: Block },
}

impl RepairEvent {
    pub fn slot(&self) -> Slot {
        match self {
            RepairEvent::FetchBlock { block } => block.slot,
        }
    }
}

/// Event sent to replay_stage when a bank needs to be switched as a result of a ParentReady.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum SwitchBankEvent {
    /// We need to switch any existing banks to this bank including ancestors.
    Switch { block: Block },
}

impl SwitchBankEvent {
    pub fn block(&self) -> Block {
        match self {
            SwitchBankEvent::Switch { block } => *block,
        }
    }
}

/// Shared single-cell holding the most recent switch-bank request from votor to replay.
///
/// Used instead of a channel because replay only ever acts on the latest event — buffering
/// older events doesn't help, and blocking on a full channel could lead to a stall in Votor.
/// Writer (votor) advances monotonically via [`Self::try_advance`]; reader (replay) pulls the
/// current value via [`Self::take`].
#[derive(Clone, Default)]
pub struct LatestSwitchRequest(Arc<Mutex<Option<SwitchBankEvent>>>);

impl LatestSwitchRequest {
    /// Records `event` as the latest pending request, iff it is strictly newer than what's
    /// currently held. Returns the previous value (if any) when it was overwritten.
    pub fn try_advance(&self, event: SwitchBankEvent) -> Option<SwitchBankEvent> {
        let mut guard = self.0.lock().unwrap();
        match guard.as_ref() {
            Some(cur) if event <= *cur => None,
            _ => guard.replace(event),
        }
    }

    /// Atomically takes the current request, leaving the cell empty.
    pub fn take(&self) -> Option<SwitchBankEvent> {
        self.0.lock().unwrap().take()
    }
}
