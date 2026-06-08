//! Vote data types for use by clients
use {
    crate::consensus_message::Block,
    serde::{Deserialize, Serialize},
    solana_clock::Slot,
    solana_hash::Hash,
    wincode::{SchemaRead, SchemaWrite},
};

/// Enum that clients can use to parse and create the vote
/// structures expected by the program
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, AbiEnumVisitor),
    frozen_abi(digest = "Fd13KXQMkc1mCJEoHwyXWkcewqBCdRcAiMhS7Aqe4sm1")
)]
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, SchemaWrite, SchemaRead,
)]
pub enum Vote {
    /// A notarization vote
    Notarize(NotarizationVote),
    /// A finalization vote
    Finalize(FinalizationVote),
    /// A skip vote
    Skip(SkipVote),
    /// A notarization fallback vote
    NotarizeFallback(NotarizationFallbackVote),
    /// A skip fallback vote
    SkipFallback(SkipFallbackVote),
    /// A genesis vote, only used during the TowerBFT -> Alpenglow Migration
    Genesis(GenesisVote),
}

/// Enum of different types of [`Vote`]s.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VoteType {
    /// Finalize vote.
    Finalize,
    /// Notarize vote.
    Notarize,
    /// Notarize fallback vote.
    NotarizeFallback,
    /// Skip vote
    Skip,
    /// Skip fallback vote.
    SkipFallback,
    /// Genesis vote.
    Genesis,
}

impl Vote {
    /// Create a new notarization vote
    pub fn new_notarization_vote(block: Block) -> Self {
        Self::from(NotarizationVote { block })
    }

    /// Create a new finalization vote
    pub fn new_finalization_vote(slot: Slot) -> Self {
        Self::from(FinalizationVote { slot })
    }

    /// Create a new skip vote
    pub fn new_skip_vote(slot: Slot) -> Self {
        Self::from(SkipVote { slot })
    }

    /// Create a new notarization fallback vote
    pub fn new_notarization_fallback_vote(block: Block) -> Self {
        Self::from(NotarizationFallbackVote { block })
    }

    /// Create a new skip fallback vote
    pub fn new_skip_fallback_vote(slot: Slot) -> Self {
        Self::from(SkipFallbackVote { slot })
    }

    /// Create a new genesis vote
    pub fn new_genesis_vote(block: Block) -> Self {
        Self::from(GenesisVote { block })
    }

    /// The slot which was voted for
    pub fn slot(&self) -> Slot {
        match self {
            Self::Notarize(vote) => vote.block.slot,
            Self::Finalize(vote) => vote.slot,
            Self::Skip(vote) => vote.slot,
            Self::NotarizeFallback(vote) => vote.block.slot,
            Self::SkipFallback(vote) => vote.slot,
            Self::Genesis(vote) => vote.block.slot,
        }
    }

    /// The block id associated with the block which was voted for
    pub fn block_id(&self) -> Option<&Hash> {
        match self {
            Self::Notarize(vote) => Some(&vote.block.block_id),
            Self::NotarizeFallback(vote) => Some(&vote.block.block_id),
            Self::Genesis(vote) => Some(&vote.block.block_id),
            Self::Finalize(_) | Self::Skip(_) | Self::SkipFallback(_) => None,
        }
    }

    /// Whether the vote is a notarization vote
    pub fn is_notarization(&self) -> bool {
        matches!(self, Self::Notarize(_))
    }

    /// Whether the vote is a finalization vote
    pub fn is_finalize(&self) -> bool {
        matches!(self, Self::Finalize(_))
    }

    /// Whether the vote is a skip vote
    pub fn is_skip(&self) -> bool {
        matches!(self, Self::Skip(_))
    }

    /// Whether the vote is a notarization fallback vote
    pub fn is_notarize_fallback(&self) -> bool {
        matches!(self, Self::NotarizeFallback(_))
    }

    /// Whether the vote is a skip fallback vote
    pub fn is_skip_fallback(&self) -> bool {
        matches!(self, Self::SkipFallback(_))
    }

    /// Whether the vote is a genesis vote
    pub fn is_genesis_vote(&self) -> bool {
        matches!(self, Self::Genesis(_))
    }

    /// Returns the [`VoteType`] for the vote.
    pub fn get_type(&self) -> VoteType {
        match self {
            Vote::Notarize(_) => VoteType::Notarize,
            Vote::NotarizeFallback(_) => VoteType::NotarizeFallback,
            Vote::Skip(_) => VoteType::Skip,
            Vote::SkipFallback(_) => VoteType::SkipFallback,
            Vote::Finalize(_) => VoteType::Finalize,
            Vote::Genesis(_) => VoteType::Genesis,
        }
    }
}

impl From<NotarizationVote> for Vote {
    fn from(vote: NotarizationVote) -> Self {
        Self::Notarize(vote)
    }
}

impl From<FinalizationVote> for Vote {
    fn from(vote: FinalizationVote) -> Self {
        Self::Finalize(vote)
    }
}

impl From<SkipVote> for Vote {
    fn from(vote: SkipVote) -> Self {
        Self::Skip(vote)
    }
}

impl From<NotarizationFallbackVote> for Vote {
    fn from(vote: NotarizationFallbackVote) -> Self {
        Self::NotarizeFallback(vote)
    }
}

impl From<SkipFallbackVote> for Vote {
    fn from(vote: SkipFallbackVote) -> Self {
        Self::SkipFallback(vote)
    }
}

impl From<GenesisVote> for Vote {
    fn from(vote: GenesisVote) -> Self {
        Self::Genesis(vote)
    }
}

/// A notarization vote
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample),
    frozen_abi(digest = "F9veHPmSwMyrYNSVuBLcvLGYSLgc7voTD3kUhxUHUTRU")
)]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    Default,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
)]
pub struct NotarizationVote {
    /// The block this vote is cast for
    pub block: Block,
}

/// A finalization vote
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample),
    frozen_abi(digest = "2XQ5N6YLJjF28w7cMFFUQ9SDgKuf9JpJNtAiXSPA8vR2")
)]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    Default,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
)]
pub struct FinalizationVote {
    /// The slot this vote is cast for.
    pub slot: Slot,
}

/// A skip vote
/// Represents a range of slots to skip
/// inclusive on both ends
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample),
    frozen_abi(digest = "G8Nrx3sMYdnLpHsCNark3BGA58BmW2sqNnqjkYhQHtN")
)]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    Default,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
)]
pub struct SkipVote {
    /// The slot this vote is cast for.
    pub slot: Slot,
}

/// A notarization fallback vote
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample),
    frozen_abi(digest = "6UW4zutbRvyri4z8WAyKx8aUZkJrZX4XoiqC4XMUnUZk")
)]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    Default,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
)]
pub struct NotarizationFallbackVote {
    /// The block this vote is cast for
    pub block: Block,
}

/// A skip fallback vote
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample),
    frozen_abi(digest = "WsUNum8V62gjRU1yAnPuBMAQui4YvMwD1RwrzHeYkeF")
)]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    Default,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
)]
pub struct SkipFallbackVote {
    /// The slot this vote is cast for.
    pub slot: Slot,
}

/// A genesis vote. Only used during the migration from TowerBFT
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample),
    frozen_abi(digest = "8ty2gETfpyVGPNMYrEFS1YXeDRprfZaisSAmJwoAYusb")
)]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    Default,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
)]
pub struct GenesisVote {
    /// The block this vote is cast for
    pub block: Block,
}
