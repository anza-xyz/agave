use {
    agave_votor_messages::{consensus_message::CertificateType, vote::Vote},
    std::time::Duration,
};

// Core consensus types and constants
pub type Stake = u64;

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VoteType {
    Finalize,
    Notarize,
    NotarizeFallback,
    Skip,
    SkipFallback,
}

impl VoteType {
    pub fn get_type(vote: &Vote) -> VoteType {
        match vote {
            Vote::Notarize(_) => VoteType::Notarize,
            Vote::NotarizeFallback(_) => VoteType::NotarizeFallback,
            Vote::Skip(_) => VoteType::Skip,
            Vote::SkipFallback(_) => VoteType::SkipFallback,
            Vote::Finalize(_) => VoteType::Finalize,
        }
    }

    #[allow(dead_code)]
    pub fn is_notarize_type(&self) -> bool {
        matches!(self, Self::Notarize | Self::NotarizeFallback)
    }
}

/// For a given [`CertificateType`], returns the fractional stake, the [`Vote`], and the optional fallback [`Vote`] required to construct it.
///
/// Must be in sync with [`vote_to_certificate_ids`].
pub(crate) fn certificate_limits_and_votes(
    cert_type: &CertificateType,
) -> (f64, Vote, Option<Vote>) {
    match cert_type {
        CertificateType::Notarize(slot, block_id) => {
            (0.6, Vote::new_notarization_vote(*slot, *block_id), None)
        }
        CertificateType::NotarizeFallback(slot, block_id) => (
            0.6,
            Vote::new_notarization_vote(*slot, *block_id),
            Some(Vote::new_notarization_fallback_vote(*slot, *block_id)),
        ),
        CertificateType::FinalizeFast(slot, block_id) => {
            (0.8, Vote::new_notarization_vote(*slot, *block_id), None)
        }
        CertificateType::Finalize(slot) => (0.6, Vote::new_finalization_vote(*slot), None),
        CertificateType::Skip(slot) => (
            0.6,
            Vote::new_skip_vote(*slot),
            Some(Vote::new_skip_fallback_vote(*slot)),
        ),
    }
}

/// Lookup from `Vote` to the `CertificateId`s the vote accounts for
///
/// Must be in sync with `certificate_limits_and_vote_types` and `VoteType::get_type`
pub fn vote_to_certificate_ids(vote: &Vote) -> Vec<CertificateType> {
    match vote {
        Vote::Notarize(vote) => vec![
            CertificateType::Notarize(vote.slot, vote.block_id),
            CertificateType::NotarizeFallback(vote.slot, vote.block_id),
            CertificateType::FinalizeFast(vote.slot, vote.block_id),
        ],
        Vote::NotarizeFallback(vote) => {
            vec![CertificateType::NotarizeFallback(vote.slot, vote.block_id)]
        }
        Vote::Finalize(vote) => vec![CertificateType::Finalize(vote.slot)],
        Vote::Skip(vote) => vec![CertificateType::Skip(vote.slot)],
        Vote::SkipFallback(vote) => vec![CertificateType::Skip(vote.slot)],
    }
}

pub const MAX_ENTRIES_PER_PUBKEY_FOR_OTHER_TYPES: usize = 1;
pub const MAX_ENTRIES_PER_PUBKEY_FOR_NOTARIZE_LITE: usize = 3;

pub const SAFE_TO_NOTAR_MIN_NOTARIZE_ONLY: f64 = 0.4;
pub const SAFE_TO_NOTAR_MIN_NOTARIZE_FOR_NOTARIZE_OR_SKIP: f64 = 0.2;
pub const SAFE_TO_NOTAR_MIN_NOTARIZE_AND_SKIP: f64 = 0.6;

pub const SAFE_TO_SKIP_THRESHOLD: f64 = 0.4;

/// Time bound assumed on network transmission delays during periods of synchrony.
pub(crate) const DELTA: Duration = Duration::from_millis(250);

/// Time the leader has for producing and sending the block.
pub(crate) const DELTA_BLOCK: Duration = Duration::from_millis(400);

/// Base timeout for when leader's first slice should arrive if they sent it immediately.
pub(crate) const DELTA_TIMEOUT: Duration = DELTA.checked_mul(3).unwrap();

/// Timeout for standstill detection mechanism.
pub(crate) const DELTA_STANDSTILL: Duration = Duration::from_millis(10_000);

/// Returns the Duration for when the `SkipTimer` should be set for for the given slot in the leader window.
#[inline]
pub fn skip_timeout(leader_block_index: usize) -> Duration {
    DELTA_TIMEOUT
        .saturating_add(
            DELTA_BLOCK
                .saturating_mul(leader_block_index as u32)
                .saturating_add(DELTA_TIMEOUT),
        )
        .saturating_add(DELTA)
}

/// Block timeout, when we should publish the final shred for the leader block index
/// within the leader window
#[inline]
pub fn block_timeout(leader_block_index: usize) -> Duration {
    // TODO: based on testing, perhaps adjust this
    DELTA_BLOCK.saturating_mul((leader_block_index as u32).saturating_add(1))
}
