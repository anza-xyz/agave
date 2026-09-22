//! Definitions related to consensus metrics collection.

use {
    crate::{VoteAccountPubkeys, vote::Vote},
    crossbeam_channel::{Receiver, Sender},
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    std::time::Instant,
};

#[derive(Debug, PartialEq, Eq)]
/// Different types of events to notify the metrics container of.
pub enum ConsensusMetricsEvent {
    /// A vote was received from the node with `id`.
    Vote {
        /// The validator that voted.
        ids: VoteAccountPubkeys,
        /// The type of vote.
        vote: Vote,
    },
    /// A block for `slot` that was produced by `leader` finished replaying.
    ReplayCompleted {
        /// The leader that produced the block.
        leader: Pubkey,
        /// The slot the block was produced for.
        slot: Slot,
    },
    /// ParentReady event was seen.
    ParentReadySeen {
        /// The slot for which the parent ready event was seen.
        slot: Slot,
    },
    /// A slot was finalized.
    SlotFinalized {
        /// The slot that was finalized.
        slot: Slot,
    },
}

/// Send side of the channel to send metrics events on.
pub type ConsensusMetricsEventSender = Sender<(Instant, ConsensusMetricsEvent)>;
/// Receive side of the channel to receive metrics events on.
pub type ConsensusMetricsEventReceiver = Receiver<(Instant, ConsensusMetricsEvent)>;

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
