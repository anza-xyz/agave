use {
    agave_votor_messages::{
        consensus_message::CertificateType,
        fraction::Fraction,
        vote::{Vote, VoteType},
    },
    std::time::Duration,
};

// Core consensus types and constants
pub type Stake = u64;

pub const fn conflicting_types(vote_type: VoteType) -> &'static [VoteType] {
    match vote_type {
        VoteType::Finalize => &[
            VoteType::NotarizeFallback,
            VoteType::Skip,
            VoteType::SkipFallback,
        ],
        VoteType::Notarize => &[VoteType::Skip, VoteType::NotarizeFallback],
        VoteType::NotarizeFallback => &[VoteType::Finalize, VoteType::Notarize],
        VoteType::Skip => &[
            VoteType::Finalize,
            VoteType::Notarize,
            VoteType::SkipFallback,
        ],
        VoteType::SkipFallback => &[VoteType::Skip, VoteType::Finalize],
        VoteType::Genesis => &[
            VoteType::Finalize,
            VoteType::Notarize,
            VoteType::NotarizeFallback,
            VoteType::Skip,
            VoteType::SkipFallback,
        ],
    }
}

/// Lookup from `Vote` to the `CertificateId`s the vote accounts for
///
/// Must be in sync with `certificate_limits_and_vote_types` and `VoteType::get_type`
pub fn vote_to_cert_types(vote: &Vote) -> Vec<CertificateType> {
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
        Vote::Genesis(vote) => vec![CertificateType::Genesis(vote.slot, vote.block_id)],
    }
}

pub const MAX_ENTRIES_PER_PUBKEY_FOR_OTHER_TYPES: usize = 1;
pub const MAX_ENTRIES_PER_PUBKEY_FOR_NOTARIZE_LITE: usize = 3;
pub const MAX_NOTAR_FALLBACK_BLOCKS: usize = 7;

pub const SAFE_TO_NOTAR_MIN_NOTARIZE_ONLY: Fraction = Fraction::from_percentage(40);
pub const SAFE_TO_NOTAR_MIN_NOTARIZE_FOR_NOTARIZE_OR_SKIP: Fraction = Fraction::from_percentage(20);
pub const SAFE_TO_NOTAR_MIN_NOTARIZE_AND_SKIP: Fraction = Fraction::from_percentage(60);

pub const SAFE_TO_SKIP_THRESHOLD: Fraction = Fraction::from_percentage(40);

/// Time bound assumed on network transmission delays during periods of synchrony.
pub const DELTA: Duration = Duration::from_millis(250);

/// Time bound for propagation delay in the block propagation sub-protocol. For
/// Turbine this is a maximum of `3 * DELTA` for the current maximum number of
/// validators.
const DELTA_BLOCK_PROPAGATION: Duration = DELTA.checked_mul(3).unwrap();

/// Base timeout for when leader's first slice should arrive if they sent it immediately.
pub(crate) const DELTA_TIMEOUT: Duration = DELTA.checked_add(DELTA_BLOCK_PROPAGATION).unwrap();

/// Timeout for standstill detection mechanism.
pub(crate) const DELTA_STANDSTILL: Duration = Duration::from_millis(10_000);
