//! Put Alpenglow consensus messages here so all clients can agree on the format.
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

/// A consensus vote.
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample),
    frozen_abi(digest = "A9wHKYuPgAR7cxidTT51ACVv5WNqHkfj2jVqJLGBC5bv")
)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VoteMessage {
    /// The type of the vote.
    pub vote: Vote,
    /// The signature.
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
            Self::Finalize(slot)
            | Self::FinalizeFast(slot, _)
            | Self::Notarize(slot, _)
            | Self::NotarizeFallback(slot, _)
            | Self::Skip(slot)
            | Self::Genesis(slot, _) => *slot,
        }
    }

    /// Gets the block associated with this certificate, if present
    pub fn to_block(self) -> Option<Block> {
        match self {
            Self::Finalize(_) | Self::Skip(_) => None,
            Self::Notarize(slot, block_id)
            | Self::NotarizeFallback(slot, block_id)
            | Self::FinalizeFast(slot, block_id)
            | Self::Genesis(slot, block_id) => Some((slot, block_id)),
        }
    }
}

/// The actual certificate with the aggregate signature and bitmap for which validators are included in the aggregate.
/// BLS vote message, we need rank to look up pubkey
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample),
    frozen_abi(digest = "CLJbmbTECu2MeBmqWNDsfTgkAC2yudxHsmNU9saww8L")
)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Certificate {
    /// The certificate type.
    pub cert_type: CertificateType,
    /// The aggregate signature.
    pub signature: BLSSignature,
    /// A rank bitmap for validators' signatures included in the aggregate.
    /// See solana-signer-store for encoding format.
    pub bitmap: Vec<u8>,
}

/// A consensus message sent between validators.
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, AbiEnumVisitor),
    frozen_abi(digest = "4YvBgNbve59tf9i4DSraiSZ3eoMF4Y1V5mDdUCoFv8S2")
)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
}
