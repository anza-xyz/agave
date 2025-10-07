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
pub enum Certificate {
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
}

/// Certificate type
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
pub enum CertificateType {
    /// Finalize certificate
    Finalize,
    /// Fast finalize certificate
    FinalizeFast,
    /// Notarize certificate
    Notarize,
    /// Notarize fallback certificate
    NotarizeFallback,
    /// Skip certificate
    Skip,
}

impl Certificate {
    /// Create a new certificate ID from a CertificateType, Option<Slot>, and Option<Hash>
    pub fn new(certificate_type: CertificateType, slot: Slot, hash: Option<Hash>) -> Self {
        match (certificate_type, hash) {
            (CertificateType::Finalize, None) => Certificate::Finalize(slot),
            (CertificateType::FinalizeFast, Some(hash)) => Certificate::FinalizeFast(slot, hash),
            (CertificateType::Notarize, Some(hash)) => Certificate::Notarize(slot, hash),
            (CertificateType::NotarizeFallback, Some(hash)) => {
                Certificate::NotarizeFallback(slot, hash)
            }
            (CertificateType::Skip, None) => Certificate::Skip(slot),
            _ => panic!("Invalid certificate type and hash combination"),
        }
    }

    /// Get the certificate type
    pub fn certificate_type(&self) -> CertificateType {
        match self {
            Certificate::Finalize(_) => CertificateType::Finalize,
            Certificate::FinalizeFast(_, _) => CertificateType::FinalizeFast,
            Certificate::Notarize(_, _) => CertificateType::Notarize,
            Certificate::NotarizeFallback(_, _) => CertificateType::NotarizeFallback,
            Certificate::Skip(_) => CertificateType::Skip,
        }
    }

    /// Get the slot of the certificate
    pub fn slot(&self) -> Slot {
        match self {
            Certificate::Finalize(slot)
            | Certificate::FinalizeFast(slot, _)
            | Certificate::Notarize(slot, _)
            | Certificate::NotarizeFallback(slot, _)
            | Certificate::Skip(slot) => *slot,
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

    /// Gets the block associated with this certificate, if present
    pub fn to_block(self) -> Option<Block> {
        match self {
            Certificate::Finalize(_) | Certificate::Skip(_) => None,
            Certificate::Notarize(slot, block_id)
            | Certificate::NotarizeFallback(slot, block_id)
            | Certificate::FinalizeFast(slot, block_id) => Some((slot, block_id)),
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
    pub fn to_source_vote(&self) -> Vote {
        match self {
            Certificate::Notarize(slot, hash) => Vote::new_notarization_vote(*slot, *hash),
            Certificate::FinalizeFast(slot, hash) => Vote::new_notarization_vote(*slot, *hash),
            Certificate::Finalize(slot) => Vote::new_finalization_vote(*slot),
            Certificate::NotarizeFallback(slot, hash) => Vote::new_notarization_vote(*slot, *hash),
            Certificate::Skip(slot) => Vote::new_skip_vote(*slot),
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
    pub fn to_source_votes(&self) -> Option<(Vote, Vote)> {
        match self {
            Certificate::NotarizeFallback(slot, hash) => {
                let vote1 = Vote::new_notarization_vote(*slot, *hash);
                let vote2 = Vote::new_notarization_fallback_vote(*slot, *hash);
                Some((vote1, vote2))
            }
            Certificate::Skip(slot) => {
                let vote1 = Vote::new_skip_vote(*slot);
                let vote2 = Vote::new_skip_fallback_vote(*slot);
                Some((vote1, vote2))
            }
            // Other certificate types do not use Base3 encoding.
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
/// BLS vote message, we need rank to look up pubkey
pub struct CertificateMessage {
    /// The certificate
    pub certificate: Certificate,
    /// The signature
    pub signature: BLSSignature,
    /// The bitmap for validators, see solana-signer-store for encoding format
    pub bitmap: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
/// BLS message data in Alpenglow
pub enum ConsensusMessage {
    /// Vote message, with the vote and the rank of the validator.
    Vote(VoteMessage),
    /// Certificate message
    Certificate(CertificateMessage),
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

    /// Create a new certificate message
    pub fn new_certificate(
        certificate: Certificate,
        bitmap: Vec<u8>,
        signature: BLSSignature,
    ) -> Self {
        Self::Certificate(CertificateMessage {
            certificate,
            signature,
            bitmap,
        })
    }
}
