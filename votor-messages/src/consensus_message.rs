//! Put BLS message here so all clients can agree on the format
use {
    crate::vote::Vote,
    serde::{Deserialize, Serialize},
    solana_bls_signatures::Signature as BLSSignature,
    solana_clock::Slot,
    solana_hash::Hash,
};

/// The seed used to derive the BLS keypair
pub const BLS_KEYPAIR_DERIVE_SEED: &[u8; 9] = b"alpenglow";

/// Block, a (slot, hash) tuple
pub type Block = (Slot, Hash);

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
/// BLS vote message, we need rank to look up pubkey
pub struct VoteMessage {
    /// The vote
    pub vote: Vote,
    /// The signature
    pub signature: BLSSignature,
    /// The rank of the validator
    pub rank: u16,
}

/// Certificate details
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
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

    /// "Critical" certs are the certificates necessary to make progress
    /// We do not consider the next slot for voting until we've seen either
    /// a Skip certificate or a NotarizeFallback certificate for ParentReady
    ///
    /// Note: Notarization certificates necessarily generate a
    /// NotarizeFallback certificate as well
    pub fn is_critical(&self) -> bool {
        matches!(self, Self::NotarizeFallback(_, _) | Self::Skip(_))
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
}

/// Definition of a consensus certificate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Certificate {
    /// The type of the certificate.
    pub cert_type: CertificateType,
    /// The signature
    pub signature: BLSSignature,
    /// The bitmap for validators, see solana-signer-store for encoding format
    pub bitmap: Vec<u8>,
}

/// Different types of consensus messages.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
pub enum ConsensusMessage {
    /// Vote message, with the vote and the rank of the validator.
    Vote(VoteMessage),
    /// Certificate message
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
