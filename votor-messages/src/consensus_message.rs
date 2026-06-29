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

#[cfg(feature = "frozen-abi")]
fn sample_hash(rng: &mut (impl solana_frozen_abi::rand::RngCore + ?Sized)) -> Hash {
    use solana_frozen_abi::stable_abi::StableAbi;
    Hash::new_from_array(<[u8; solana_hash::HASH_BYTES] as StableAbi>::random(rng))
}

/// An alpenglow block
#[cfg_attr(feature = "frozen-abi", derive(AbiExample, StableAbi, StableAbiSample))]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
)]
#[serde(rename_all = "camelCase")]
pub struct Block {
    /// The slot in the block.
    pub slot: Slot,
    /// The block_id of the block.
    #[cfg_attr(feature = "frozen-abi", stable_abi_sample(with = "sample_hash(rng)"))]
    pub block_id: Hash,
}

/// A consensus vote.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct VoteMessage {
    /// The type of the vote.
    pub vote: Vote,
    /// The signature.
    pub signature: BLSSignature,
    /// The rank of the validator.
    pub rank: u16,
}

/// A consensus message sent between validators.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
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

    /// Returns the slot this message is for.
    pub fn slot(&self) -> Slot {
        match self {
            Self::Vote(vote) => vote.vote.slot(),
            Self::Certificate(certificate) => certificate.cert_type.slot(),
        }
    }
}

impl From<Certificate> for ConsensusMessage {
    fn from(cert: Certificate) -> Self {
        Self::Certificate(cert)
    }
}
