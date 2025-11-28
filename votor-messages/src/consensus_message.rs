//! Put Alpenglow consensus messages here so all clients can agree on the format.
use {
    crate::{slice_root::SliceRoot, vote::Vote},
    serde::{Deserialize, Serialize},
    solana_bls_signatures::Signature as BLSSignature,
    solana_clock::Slot,
};

/// The seed used to derive the BLS keypair
pub const BLS_KEYPAIR_DERIVE_SEED: &[u8; 9] = b"alpenglow";

/// Block, a (slot, hash) tuple
pub type Block = (Slot, SliceRoot);

/// A consensus vote.
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample),
    frozen_abi(digest = "9DbEK4Z9NfvEEWMhJU3kGUkXizCDxLwvo1wmhKYYwskv")
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
    frozen_abi(digest = "51ceE7GbgJqvtnkiWwuWnn3jVaonbzk3PHS6zT2BuC5j")
)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
pub enum CertificateType {
    /// Finalize certificate
    Finalize(Slot),
    /// Fast finalize certificate
    FinalizeFast(Slot, SliceRoot),
    /// Notarize certificate
    Notarize(Slot, SliceRoot),
    /// Notarize fallback certificate
    NotarizeFallback(Slot, SliceRoot),
    /// Skip certificate
    Skip(Slot),
}

impl CertificateType {
    /// Get the slot of the certificate
    pub fn slot(&self) -> Slot {
        match self {
            Self::Finalize(slot)
            | Self::FinalizeFast(slot, _)
            | Self::Notarize(slot, _)
            | Self::NotarizeFallback(slot, _)
            | Self::Skip(slot) => *slot,
        }
    }

    /// Gets the block associated with this certificate, if present
    pub fn to_block(self) -> Option<Block> {
        match self {
            Self::Finalize(_) | Self::Skip(_) => None,
            Self::Notarize(slot, block_id)
            | Self::NotarizeFallback(slot, block_id)
            | Self::FinalizeFast(slot, block_id) => Some((slot, block_id)),
        }
    }
}

/// The actual certificate with the aggregate signature and bitmap for which validators are included in the aggregate.
/// BLS vote message, we need rank to look up pubkey
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample),
    frozen_abi(digest = "HYKJv4sE12YGEnbwbyJYRb9rMtu4QL3XiA9WVGgBnV6b")
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
    frozen_abi(digest = "7aRCNJMpQu4GxNMPBKcGhEMZ4EDS5tjpCbZa5Wh41UAu")
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
