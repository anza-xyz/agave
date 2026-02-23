//! Put Alpenglow consensus messages here so all clients can agree on the format.
use {
    crate::{
        fraction::Fraction,
        migration::GENESIS_VOTE_THRESHOLD,
        vote::{Vote, VoteType},
    },
    serde::{Deserialize, Serialize},
    solana_bls_signatures::Signature as BLSSignature,
    solana_clock::Slot,
    solana_hash::Hash,
    wincode::{SchemaRead, SchemaWrite, containers::Pod},
};

/// The seed used to derive the BLS keypair
pub const BLS_KEYPAIR_DERIVE_SEED: &[u8; 9] = b"alpenglow";

/// Block, a (slot, hash) tuple
pub type Block = (Slot, Hash);
/// A consensus vote.
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample),
    frozen_abi(digest = "5eorzdc18a1sNEUDLAKPgrHCqHmA8ssuTwKSGsZLwBqR")
)]
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct VoteMessage {
    /// The type of the vote.
    pub vote: Vote,
    /// The signature.
    #[wincode(with = "Pod<BLSSignature>")]
    pub signature: BLSSignature,
    /// The rank of the validator.
    pub rank: u16,
}

/// The different types of certificates and their relevant state.
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, AbiEnumVisitor),
    frozen_abi(digest = "CazjewshYYizgQuCgBBRv6gzasJpUvFVKoSeEirWRKgA")
)]
#[derive(
    Debug,
    Copy,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Deserialize,
    Serialize,
    SchemaWrite,
    SchemaRead,
)]
pub enum CertificateType {
    /// Finalize certificate
    Finalize(Slot),
    /// Fast finalize certificate
    FinalizeFast(Slot, Hash),
    /// Notarize certificate
    Notarize(Slot, Hash),
    /// Notarize fallback certificate
    NotarizeFallback(Slot, Hash),
    /// Skip certificate
    Skip(Slot),
    /// Genesis certificate
    Genesis(Slot, Hash),
}

impl CertificateType {
    /// Get the slot of the certificate
    pub fn slot(&self) -> Slot {
        match self {
            CertificateType::Finalize(slot)
            | CertificateType::FinalizeFast(slot, _)
            | CertificateType::Notarize(slot, _)
            | CertificateType::NotarizeFallback(slot, _)
            | CertificateType::Genesis(slot, _)
            | CertificateType::Skip(slot) => *slot,
        }
    }

    /// Is this a fast finalize certificate?
    pub fn is_fast_finalization(&self) -> bool {
        matches!(self, Self::FinalizeFast(_, _))
    }

    /// Is this a finalize / fast finalize certificate?
    pub fn is_finalization(&self) -> bool {
        matches!(self, Self::Finalize(_) | Self::FinalizeFast(_, _))
    }

    /// Is this a slow finalize certificate?
    pub fn is_slow_finalization(&self) -> bool {
        matches!(self, Self::Finalize(_))
    }

    /// Is this a notarize certificate?
    pub fn is_notarize(&self) -> bool {
        matches!(self, Self::Notarize(_, _))
    }

    /// Is this a notarize fallback certificate?
    pub fn is_notarize_fallback(&self) -> bool {
        matches!(self, Self::NotarizeFallback(_, _))
    }

    /// Is this a skip certificate?
    pub fn is_skip(&self) -> bool {
        matches!(self, Self::Skip(_))
    }

    /// Is this a genesis certificate?
    pub fn is_genesis(&self) -> bool {
        matches!(self, Self::Genesis(_, _))
    }

    /// Gets the block associated with this certificate, if present
    pub fn to_block(self) -> Option<Block> {
        match self {
            CertificateType::Finalize(_) | CertificateType::Skip(_) => None,
            CertificateType::Notarize(slot, block_id)
            | CertificateType::NotarizeFallback(slot, block_id)
            | CertificateType::Genesis(slot, block_id)
            | CertificateType::FinalizeFast(slot, block_id) => Some((slot, block_id)),
        }
    }

    /// Reconstructs the single source `Vote` payload for this certificate.
    ///
    /// This method is used primarily by the signature verifier. For
    /// certificates formed by aggregating a single type of vote
    /// (e.g., a `Notarize` certificate from `Notarize` votes), this function
    /// reconstructs the canonical message payload that was signed by validators.
    ///
    /// For `NotarizeFallback` and `Skip` certificates, this function returns the
    /// appropriate payload *only* if the certificate was formed from a single
    /// vote type (e.g., exclusively from `Notarize` or `Skip` votes). For
    /// certificates formed from a mix of two vote types, use the `to_source_votes`
    /// function.
    pub fn to_source_vote(self) -> Vote {
        match self {
            Self::Notarize(slot, block_id)
            | Self::FinalizeFast(slot, block_id)
            | Self::NotarizeFallback(slot, block_id) => Vote::new_notarization_vote(slot, block_id),
            Self::Finalize(slot) => Vote::new_finalization_vote(slot),
            Self::Skip(slot) => Vote::new_skip_vote(slot),
            Self::Genesis(slot, block_id) => Vote::new_genesis_vote(slot, block_id),
        }
    }

    /// Reconstructs the two distinct source `Vote` payloads for this certificate.
    ///
    /// This method is primarily used by the signature verifier for certificates that
    /// can be formed by aggregating two different types of votes. For example, a
    /// `NotarizeFallback` certificate accepts both `Notarize` and `NotarizeFallback`.
    ///
    /// It reconstructs both potential message payloads that were signed by validators, which
    /// the verifier uses to check the single aggregate signature.
    pub fn to_source_votes(self) -> Option<(Vote, Vote)> {
        match self {
            Self::NotarizeFallback(slot, block_id) => {
                let vote1 = Vote::new_notarization_vote(slot, block_id);
                let vote2 = Vote::new_notarization_fallback_vote(slot, block_id);
                Some((vote1, vote2))
            }
            Self::Skip(slot) => {
                let vote1 = Vote::new_skip_vote(slot);
                let vote2 = Vote::new_skip_fallback_vote(slot);
                Some((vote1, vote2))
            }
            // Other certificate types do not use Base3 encoding.
            _ => None,
        }
    }

    /// Returns the stake fraction required for certificate completion and the
    /// `VoteType`s that contribute to this certificate.
    ///
    /// Must be in sync with `Vote::to_cert_types`
    pub const fn limits_and_vote_types(&self) -> (Fraction, &'static [VoteType]) {
        match self {
            CertificateType::Notarize(_, _) => {
                (Fraction::from_percentage(60), &[VoteType::Notarize])
            }
            CertificateType::NotarizeFallback(_, _) => (
                Fraction::from_percentage(60),
                &[VoteType::Notarize, VoteType::NotarizeFallback],
            ),
            CertificateType::FinalizeFast(_, _) => {
                (Fraction::from_percentage(80), &[VoteType::Notarize])
            }
            CertificateType::Finalize(_) => (Fraction::from_percentage(60), &[VoteType::Finalize]),
            CertificateType::Skip(_) => (
                Fraction::from_percentage(60),
                &[VoteType::Skip, VoteType::SkipFallback],
            ),
            CertificateType::Genesis(_, _) => (GENESIS_VOTE_THRESHOLD, &[VoteType::Genesis]),
        }
    }
}

/// The actual certificate with the aggregate signature and bitmap for which validators are included in the aggregate.
/// BLS vote message, we need rank to look up pubkey
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample),
    frozen_abi(digest = "B5NsoWZr2Lpbbjqj8udwEKvae6bA37Pm4R92udZHxwfU")
)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct Certificate {
    /// The certificate type.
    pub cert_type: CertificateType,
    /// The aggregate signature.
    #[wincode(with = "Pod<BLSSignature>")]
    pub signature: BLSSignature,
    /// A rank bitmap for validators' signatures included in the aggregate.
    /// See solana-signer-store for encoding format.
    pub bitmap: Vec<u8>,
}

/// A consensus message sent between validators.
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, AbiEnumVisitor),
    frozen_abi(digest = "BdKT6dbkLnTeGNMS8XtQkg6HTeHSQ6Z41Btc1rJ117PB")
)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead)]
#[allow(clippy::large_enum_variant)]
pub enum ConsensusMessage {
    /// A vote from a single party.
    Vote(VoteMessage),
    /// A certificate aggregating votes from multiple parties.
    Certificate(Certificate),
}

impl ConsensusMessage {
    /// Create a new vote message
    pub fn new_vote(vote: Vote, signature: BLSSignature, rank: u16) -> Self {
        Self::Vote(VoteMessage {
            vote,
            signature,
            rank,
        })
    }

    /// Create a new certificate.
    pub fn new_certificate(
        cert_type: CertificateType,
        bitmap: Vec<u8>,
        signature: BLSSignature,
    ) -> Self {
        Self::Certificate(Certificate {
            cert_type,
            signature,
            bitmap,
        })
    }
}
