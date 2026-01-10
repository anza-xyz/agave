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
    derive(AbiExample, StableAbi),
    frozen_abi(
        api_digest = "4nTsJNvSwHrs8FHz2TPbSBiYc8Eattw4v31XDge2c5zA",
        abi_digest = "EqdzDfJcBpJoN1FtahpDAR8Tmg3tbwo87RcwwsbwE28h"
    )
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

#[cfg(feature = "frozen-abi")]
impl solana_frozen_abi::rand::prelude::Distribution<VoteMessage>
    for solana_frozen_abi::rand::distr::StandardUniform
{
    fn sample<R: solana_frozen_abi::rand::Rng + ?Sized>(&self, rng: &mut R) -> VoteMessage {
        VoteMessage {
            vote: rng.random(),
            signature: solana_bls_signatures::signature::Signature(std::array::from_fn(|_| {
                rng.random::<u8>()
            })),
            rank: rng.random(),
        }
    }
}

/// The different types of certificates and their relevant state.
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, AbiEnumVisitor, StableAbi),
    frozen_abi(
        api_digest = "CazjewshYYizgQuCgBBRv6gzasJpUvFVKoSeEirWRKgA",
        abi_digest = "2Hbsm8EV5Z4TtN5JnbxEWfuZWiqsaGV6zMdRjCud97PX"
    )
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

#[cfg(feature = "frozen-abi")]
impl solana_frozen_abi::rand::prelude::Distribution<CertificateType>
    for solana_frozen_abi::rand::distr::StandardUniform
{
    fn sample<R: solana_frozen_abi::rand::Rng + ?Sized>(&self, rng: &mut R) -> CertificateType {
        match rng.random_range(0..5) {
            0 => CertificateType::Finalize(rng.random()),
            1 => CertificateType::FinalizeFast(rng.random(), Hash::new_from_array(rng.random())),
            2 => CertificateType::Notarize(rng.random(), Hash::new_from_array(rng.random())),
            3 => {
                CertificateType::NotarizeFallback(rng.random(), Hash::new_from_array(rng.random()))
            }
            4 => CertificateType::Skip(rng.random()),
            _ => unreachable!(),
        }
    }
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
    derive(AbiExample, StableAbi),
    frozen_abi(
        api_digest = "7AATkxH9takDKv4pGT3jsqC18nqEgdRTYGUP8G2bbnGp",
        abi_digest = "DBrDWdZoYysGgUczvszC7Evvo5kHr68affmwsisMKjxJ"
    )
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

#[cfg(feature = "frozen-abi")]
impl solana_frozen_abi::rand::prelude::Distribution<Certificate>
    for solana_frozen_abi::rand::distr::StandardUniform
{
    fn sample<R: solana_frozen_abi::rand::Rng + ?Sized>(&self, rng: &mut R) -> Certificate {
        Certificate {
            cert_type: rng.random(),
            signature: solana_bls_signatures::signature::Signature(std::array::from_fn(|_| {
                rng.random::<u8>()
            })),
            bitmap: (0..1000).map(|_| rng.random()).collect(),
        }
    }
}

/// A consensus message sent between validators.
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, AbiEnumVisitor, StableAbi),
    frozen_abi(
        api_digest = "DaXHFwdUAESLYZvyhsgwjeBUEeUqjp3t7WfkRsSp3cEE",
        abi_digest = "3DsaRkNcyQjFX7A5WkJScYyLyCA9QzgrDBE1fhuib2ur"
    )
)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
pub enum ConsensusMessage {
    /// A vote from a single party.
    Vote(VoteMessage),
    /// A certificate aggregating votes from multiple parties.
    Certificate(Certificate),
}

#[cfg(feature = "frozen-abi")]
impl solana_frozen_abi::rand::prelude::Distribution<ConsensusMessage>
    for solana_frozen_abi::rand::distr::StandardUniform
{
    fn sample<R: solana_frozen_abi::rand::Rng + ?Sized>(&self, rng: &mut R) -> ConsensusMessage {
        match rng.random_range(0..1) {
            0 => ConsensusMessage::Vote(rng.random()),
            _ => ConsensusMessage::Certificate(rng.random()),
        }
    }
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
