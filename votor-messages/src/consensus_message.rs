//! Put Alpenglow consensus messages here so all clients can agree on the format.
use {
    crate::{
        certificate::{Certificate, CertificateType},
        vote::Vote,
    },
    bitvec::vec::BitVec,
    serde::{Deserialize, Serialize},
    solana_bls_signatures::{
        AsSignatureProjective, Signature as BLSSignature, SignatureProjective,
    },
    solana_clock::Slot,
    solana_hash::Hash,
    std::num::NonZero,
    wincode::{SchemaRead, SchemaWrite, pod_wrapper},
};

// Use `BLSSignature` directly once `BLSSignature` wincode support
// is released in solana-sdk.
pod_wrapper! {
    unsafe struct PodBLSSignature(BLSSignature);
}

/// The seed used to derive the BLS keypair
pub const BLS_KEYPAIR_DERIVE_SEED: &[u8; 9] = b"alpenglow";

/// An alpenglow block
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample),
    frozen_abi(digest = "xCqtGMfgy9TMmCDZP9o4BidVTPKfMWrLmqxpRDLYwtR")
)]
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
pub struct Block {
    /// The slot in the block.
    pub slot: Slot,
    /// The block_id of the block.
    pub block_id: Hash,
}

/// A consensus vote.
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample),
    frozen_abi(digest = "CTiXEk2aQbpf6TS6PNKcaTsGkLruDvAYsTLFhHKW2vsm")
)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead)]
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
    frozen_abi(digest = "CbPatwRWz8NyUAj3HeAxAAWAWTxJnHGAfekLspUQpMHN")
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

/// A batch of identical votes that have been sigverified
///
/// NOTE: the fields are intentially not exposed publicly to force users to use constructors
/// thereby ensuring that the fields are set properly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigVerifiedVoteBatch {
    /// The type of vote in the batch.
    vote: Vote,
    /// The aggregate signature of the votes in the batch.
    signature: SignatureProjective,
    /// The total stake in the batch.
    stake: NonZero<u64>,
    /// Ranks of the various validators whose votes are in the batch.
    ranks: BitVec<u8>,
}

impl SigVerifiedVoteBatch {
    /// Constructs a sig verified vote batch from a list of votes
    // TODO: should this constructor only be avilable to bls sigverifier?
    pub fn new(msg: VoteMessage, stake: NonZero<u64>) -> Self {
        // TODO: handle unwrap; lookup max_validators; and look up stake
        let max_validators = 2048;
        let mut ranks = BitVec::new();
        ranks.resize(max_validators, false);
        ranks.set(msg.rank as usize, true);
        Self {
            vote: msg.vote,
            signature: msg.signature.try_as_projective().unwrap(),
            stake,
            ranks,
        }
    }

    /// Returns the ranks of the validators whose votes are in the batch.
    pub fn ranks(&self) -> &BitVec<u8> {
        &self.ranks
    }

    /// Returns the type of vote in this batch.
    pub fn vote(&self) -> &Vote {
        &self.vote
    }

    /// Returns the aggregate signature of the votes in the batch.
    pub fn signature(&self) -> &SignatureProjective {
        &self.signature
    }

    /// Returns the total stake in the batch.
    pub fn stake(&self) -> NonZero<u64> {
        self.stake
    }

    /// Returns the length of the batch
    pub fn len(&self) -> usize {
        self.ranks.count_ones()
    }

    /// Returns true if the batch is empty
    pub fn is_empty(&self) -> bool {
        self.ranks.count_ones() == 0
    }

    #[cfg(feature = "dev-context-only-utils")]
    /// Constructs a new vote batch for test purposes
    pub fn new_for_test(vote: Vote, max_validators: usize, rank: u16) -> Self {
        let mut ranks = BitVec::new();
        ranks.resize(max_validators, false);
        ranks.set(rank as usize, true);
        Self {
            vote,
            stake: NonZero::new(123).unwrap(),
            signature: SignatureProjective::identity(),
            ranks,
        }
    }
}

/// A batch of vote or cert being sent within the node.  They were either sig verified before being
/// sent or they were generated by the node itself so do not need to be sig verified.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum SigVerifiedBatch {
    /// Batch of votes.
    Votes(SigVerifiedVoteBatch),
    /// Batch of certs.
    Certificates(Vec<Certificate>),
}

impl SigVerifiedBatch {
    /// Returns the length of the batch
    pub fn len(&self) -> usize {
        match self {
            Self::Votes(votes) => votes.len(),
            Self::Certificates(certs) => certs.len(),
        }
    }

    /// Returns true if the batch is empty.
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Votes(votes) => votes.is_empty(),
            Self::Certificates(certs) => certs.is_empty(),
        }
    }
}
