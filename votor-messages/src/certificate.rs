//! Definitions of Alpenglow consensus certificates

use {
    crate::{
        consensus_message::Block, fraction::Fraction, migration::GENESIS_VOTE_THRESHOLD, vote::Vote,
    },
    solana_bls_signatures::Signature as BLSSignature,
    solana_clock::Slot,
};

#[derive(Debug, Clone, PartialEq, Eq)]
/// A cert signature
pub struct CertSignature {
    /// The aggregate signature.
    pub signature: BLSSignature,
    /// A rank bitmap for validators' signatures included in the aggregate.
    /// See solana-signer-store for encoding format.
    pub bitmap: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Genesis cert
pub struct GenesisCert {
    /// Block the cert is for.
    pub block: Block,
    /// the signature on the cert
    pub signature: CertSignature,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// A notarize cert
pub struct NotarCert {
    /// Block the cert is for.
    pub block: Block,
    /// the signature on the cert
    pub signature: CertSignature,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// A slow finalized cert
pub struct FinalizeCert {
    /// Slot the cert is for.
    pub slot: Slot,
    /// the signature on the cert
    pub signature: CertSignature,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// A fast finalized cert
pub struct FastFinalizeCert {
    /// Block the cert is for.
    pub block: Block,
    /// the signature on the cert
    pub signature: CertSignature,
}

/// The actual certificate with the aggregate signature and bitmap for which validators are included in the aggregate.
/// BLS vote message, we need rank to look up pubkey
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Certificate {
    /// The certificate type.
    pub cert_type: CertificateType,
    /// The aggregate signature.
    pub signature: BLSSignature,
    /// A rank bitmap for validators' signatures included in the aggregate.
    /// See solana-signer-store for encoding format.
    pub bitmap: Vec<u8>,
}

/// The different types of certificates and their relevant state.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CertificateType {
    /// Finalize certificate
    Finalize(Slot),
    /// Fast finalize certificate
    FinalizeFast(Block),
    /// Notarize certificate
    Notarize(Block),
    /// Notarize fallback certificate
    NotarizeFallback(Block),
    /// Skip certificate
    Skip(Slot),
    /// Genesis certificate
    Genesis(Block),
}

impl CertificateType {
    /// Get the slot of the certificate
    pub fn slot(&self) -> Slot {
        match self {
            CertificateType::Finalize(slot)
            | CertificateType::FinalizeFast(Block { slot, block_id: _ })
            | CertificateType::NotarizeFallback(Block { slot, block_id: _ })
            | CertificateType::Notarize(Block { slot, block_id: _ })
            | CertificateType::Genesis(Block { slot, block_id: _ })
            | CertificateType::Skip(slot) => *slot,
        }
    }

    /// Returns the threshold needed to complete the cert of this type.
    pub const fn threshold(&self) -> Fraction {
        match self {
            Self::Finalize(_) => Fraction::from_percentage(60),
            Self::Skip(_) => Fraction::from_percentage(60),
            Self::Notarize(_) => Fraction::from_percentage(60),
            Self::NotarizeFallback(_) => Fraction::from_percentage(60),
            Self::FinalizeFast(_) => Fraction::from_percentage(80),
            Self::Genesis(_) => GENESIS_VOTE_THRESHOLD,
        }
    }

    /// Is this a fast finalize certificate?
    pub fn is_fast_finalization(&self) -> bool {
        matches!(self, Self::FinalizeFast(_))
    }

    /// Is this a finalize / fast finalize certificate?
    pub fn is_finalization(&self) -> bool {
        matches!(self, Self::Finalize(_) | Self::FinalizeFast(_))
    }

    /// Is this a slow finalize certificate?
    pub fn is_slow_finalization(&self) -> bool {
        matches!(self, Self::Finalize(_))
    }

    /// Is this a notarize certificate?
    pub fn is_notarize(&self) -> bool {
        matches!(self, Self::Notarize(_))
    }

    /// Is this a notarize fallback certificate?
    pub fn is_notarize_fallback(&self) -> bool {
        matches!(self, Self::NotarizeFallback(_))
    }

    /// Is this a skip certificate?
    pub fn is_skip(&self) -> bool {
        matches!(self, Self::Skip(_))
    }

    /// Is this a genesis certificate?
    pub fn is_genesis(&self) -> bool {
        matches!(self, Self::Genesis(_))
    }

    /// Gets the block associated with this certificate, if present
    pub fn to_block(self) -> Option<Block> {
        match self {
            CertificateType::Finalize(_) | CertificateType::Skip(_) => None,
            CertificateType::Notarize(block)
            | CertificateType::NotarizeFallback(block)
            | CertificateType::Genesis(block)
            | CertificateType::FinalizeFast(block) => Some(block),
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
            Self::Notarize(block) | Self::FinalizeFast(block) | Self::NotarizeFallback(block) => {
                Vote::new_notarization_vote(block)
            }
            Self::Finalize(slot) => Vote::new_finalization_vote(slot),
            Self::Skip(slot) => Vote::new_skip_vote(slot),
            Self::Genesis(block) => Vote::new_genesis_vote(block),
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
            Self::NotarizeFallback(block) => {
                let vote1 = Vote::new_notarization_vote(block);
                let vote2 = Vote::new_notarization_fallback_vote(block);
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
}
