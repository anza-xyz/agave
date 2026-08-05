//! Definitions related to consensus metrics collection.

use {
    crate::vote::Vote,
    crossbeam_channel::{Receiver, Sender},
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    std::time::Instant,
};

/// Different types of events to notify the metrics container of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsensusMetricsEvent {
    /// A vote was received from the node with `id`.
    Vote {
        /// The validator that voted.
        id: Pubkey,
        /// The type of vote.
        vote: Vote,
    },
    /// A block hash was seen for `slot` and the `leader` is responsible for producing it.
    BlockHashSeen {
        /// The leader that produced the block.
        leader: Pubkey,
        /// The slot the block was produced for.
        slot: Slot,
    },
    /// Start of slot.
    StartOfSlot {
        /// The slot that just started.
        slot: Slot,
    },
    /// A slot was finalized.
    SlotFinalized {
        /// The slot that was finalized.
        slot: Slot,
    },
}

/// Send side of the channel to send metrics events on.
pub type ConsensusMetricsEventSender = Sender<(Instant, Vec<ConsensusMetricsEvent>)>;
/// Receive side of the channel to receive metrics events on.
pub type ConsensusMetricsEventReceiver = Receiver<(Instant, Vec<ConsensusMetricsEvent>)>;

/// Even at 10 events per slot, this supports 1000 slots in flight
/// With 2000 active validators, we can't have more than:
/// - 1 Notarize vote
/// - 3 Notarize-fallback votes
/// - 1 Skip-fallback vote
/// - 1 Finalize vote
///
/// Per validator, resulting in 12k vote events.
/// We overprovision this channel at 15k total events.
pub const MAX_IN_FLIGHT_CONSENSUS_EVENTS: usize = 15_000;
