//! Put Alpenglow consensus messages here so all clients can agree on the format.
use {
    crate::{
        certificate::{Certificate, CertificateType},
        vote::Vote,
    },
    serde::{Deserialize, Serialize},
    solana_bls_signatures::Signature as BLSSignature,
    solana_clock::Slot,
    solana_hash::Hash,
    wincode::{SchemaRead, SchemaWrite, pod_wrapper},
};

// Use `BLSSignature` directly once `BLSSignature` wincode support
// is released in solana-sdk.
pod_wrapper! {
    unsafe struct PodBLSSignature(BLSSignature);
}

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
    #[wincode(with = "PodBLSSignature")]
    pub signature: BLSSignature,
    /// The rank of the validator.
    pub rank: u16,
}

/// A consensus message sent between validators.
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, AbiEnumVisitor),
    frozen_abi(digest = "FJKdeeziPmtNpqrV7Wk3tVeHS6CzWWLvkX3MdgYXoKwk")
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

impl From<Certificate> for ConsensusMessage {
    fn from(cert: Certificate) -> Self {
        Self::Certificate(cert)
    }
}
