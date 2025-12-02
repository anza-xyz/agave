//! Vote data types for use by clients
use {
    serde::{Deserialize, Serialize},
    solana_clock::Slot,
    solana_hash::Hash,
};

/// Enum that clients can use to parse and create the vote
/// structures expected by the program
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, AbiEnumVisitor, StableAbi),
    frozen_abi(
        api_digest = "6NFC2nmHc5VdjYKn6cbiskj9cLyk7jsWErJinojzYQhX",
        abi_digest = "42YgWB8k6nErZJWqmcxN4QoRBVoeTHTymgDS99HdDUfz"
    )
)]
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
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
}

#[cfg(feature = "frozen-abi")]
impl solana_frozen_abi::rand::prelude::Distribution<Vote>
    for solana_frozen_abi::rand::distributions::Standard
{
    fn sample<R: solana_frozen_abi::rand::Rng + ?Sized>(&self, rng: &mut R) -> Vote {
        match rng.r#gen_range(0..5) {
            0 => Vote::Notarize(rng.r#gen()),
            1 => Vote::Finalize(rng.r#gen()),
            2 => Vote::Skip(rng.r#gen()),
            3 => Vote::NotarizeFallback(rng.r#gen()),
            _ => Vote::SkipFallback(rng.r#gen()),
        }
    }
}

impl Vote {
    /// Create a new notarization vote
    pub fn new_notarization_vote(slot: Slot, block_id: Hash) -> Self {
        Self::from(NotarizationVote { slot, block_id })
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
    pub fn new_notarization_fallback_vote(slot: Slot, block_id: Hash) -> Self {
        Self::from(NotarizationFallbackVote { slot, block_id })
    }

    /// Create a new skip fallback vote
    pub fn new_skip_fallback_vote(slot: Slot) -> Self {
        Self::from(SkipFallbackVote { slot })
    }

    /// The slot which was voted for
    pub fn slot(&self) -> Slot {
        match self {
            Self::Notarize(vote) => vote.slot,
            Self::Finalize(vote) => vote.slot,
            Self::Skip(vote) => vote.slot,
            Self::NotarizeFallback(vote) => vote.slot,
            Self::SkipFallback(vote) => vote.slot,
        }
    }

    /// The block id associated with the block which was voted for
    pub fn block_id(&self) -> Option<&Hash> {
        match self {
            Self::Notarize(vote) => Some(&vote.block_id),
            Self::NotarizeFallback(vote) => Some(&vote.block_id),
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

    /// Whether the vote is a notarization or finalization
    pub fn is_notarization_or_finalization(&self) -> bool {
        matches!(self, Self::Notarize(_) | Self::Finalize(_))
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

/// A notarization vote
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, StableAbi),
    frozen_abi(
        api_digest = "5AdwChAjsj5QUXLdpDnGGK2L2nA8y8EajVXi6jsmTv1m",
        abi_digest = "AL14y9AbENos7pYdc6ji6RmcgKUcwQCb1YUfaCUveRtL"
    )
)]
#[derive(Clone, Copy, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct NotarizationVote {
    /// The slot this vote is cast for.
    pub slot: Slot,
    /// The block id this vote is for.
    pub block_id: Hash,
}

#[cfg(feature = "frozen-abi")]
impl solana_frozen_abi::rand::prelude::Distribution<NotarizationVote>
    for solana_frozen_abi::rand::distributions::Standard
{
    fn sample<R: solana_frozen_abi::rand::Rng + ?Sized>(&self, rng: &mut R) -> NotarizationVote {
        NotarizationVote {
            slot: rng.r#gen(),
            block_id: Hash::new_from_array(rng.r#gen()),
        }
    }
}

/// A finalization vote
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, StableAbi),
    frozen_abi(
        api_digest = "2XQ5N6YLJjF28w7cMFFUQ9SDgKuf9JpJNtAiXSPA8vR2",
        abi_digest = "HXz8236KK4xi9WaHQxPpGyrsp3AjdBaCWMQiwVhcKv83"
    )
)]
#[derive(Clone, Copy, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct FinalizationVote {
    /// The slot this vote is cast for.
    pub slot: Slot,
}

#[cfg(feature = "frozen-abi")]
impl solana_frozen_abi::rand::prelude::Distribution<FinalizationVote>
    for solana_frozen_abi::rand::distributions::Standard
{
    fn sample<R: solana_frozen_abi::rand::Rng + ?Sized>(&self, rng: &mut R) -> FinalizationVote {
        FinalizationVote { slot: rng.r#gen() }
    }
}

/// A skip vote
/// Represents a range of slots to skip
/// inclusive on both ends
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, StableAbi),
    frozen_abi(
        api_digest = "G8Nrx3sMYdnLpHsCNark3BGA58BmW2sqNnqjkYhQHtN",
        abi_digest = "HXz8236KK4xi9WaHQxPpGyrsp3AjdBaCWMQiwVhcKv83"
    )
)]
#[derive(Clone, Copy, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct SkipVote {
    /// The slot this vote is cast for.
    pub slot: Slot,
}

#[cfg(feature = "frozen-abi")]
impl solana_frozen_abi::rand::prelude::Distribution<SkipVote>
    for solana_frozen_abi::rand::distributions::Standard
{
    fn sample<R: solana_frozen_abi::rand::Rng + ?Sized>(&self, rng: &mut R) -> SkipVote {
        SkipVote { slot: rng.r#gen() }
    }
}

/// A notarization fallback vote
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, StableAbi),
    frozen_abi(
        api_digest = "7j5ZPwwyz1FaG3fpyQv5PVnQXicdSmqSk8NvqzkG1Eqz",
        abi_digest = "AL14y9AbENos7pYdc6ji6RmcgKUcwQCb1YUfaCUveRtL"
    )
)]
#[derive(Clone, Copy, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct NotarizationFallbackVote {
    /// The slot this vote is cast for.
    pub slot: Slot,
    /// The block id this vote is for.
    pub block_id: Hash,
}

#[cfg(feature = "frozen-abi")]
impl solana_frozen_abi::rand::prelude::Distribution<NotarizationFallbackVote>
    for solana_frozen_abi::rand::distributions::Standard
{
    fn sample<R: solana_frozen_abi::rand::Rng + ?Sized>(
        &self,
        rng: &mut R,
    ) -> NotarizationFallbackVote {
        NotarizationFallbackVote {
            slot: rng.r#gen(),
            block_id: Hash::new_from_array(rng.r#gen()),
        }
    }
}

/// A skip fallback vote
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, StableAbi),
    frozen_abi(
        api_digest = "WsUNum8V62gjRU1yAnPuBMAQui4YvMwD1RwrzHeYkeF",
        abi_digest = "HXz8236KK4xi9WaHQxPpGyrsp3AjdBaCWMQiwVhcKv83"
    )
)]
#[derive(Clone, Copy, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct SkipFallbackVote {
    /// The slot this vote is cast for.
    pub slot: Slot,
}

#[cfg(feature = "frozen-abi")]
impl solana_frozen_abi::rand::prelude::Distribution<SkipFallbackVote>
    for solana_frozen_abi::rand::distributions::Standard
{
    fn sample<R: solana_frozen_abi::rand::Rng + ?Sized>(&self, rng: &mut R) -> SkipFallbackVote {
        SkipFallbackVote { slot: rng.r#gen() }
    }
}
