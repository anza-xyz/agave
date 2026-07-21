//! This module defines various types relevant to the wire format of alpenglow
//! votes and certificates.
//!
//! When a validator wants to send a `Vote` to other nodes, it needs to include
//! the `shred_version` in the signed payload so that other validators can
//! ensure that they are both in the same cluster instance.  To get the
//! appropriate vote payload with the shred version, the sender should use
//! `get_vote_payload_to_sign()`.  The shred version does not need to be part of
//! the vote payload.  The receiver can use `get_vote_payload_to_sign()` with
//! its own shred_version to get the payload that should be verified.  If the
//! signature does not match, then the vote should be rejected.  Once the sender
//! has the signature on the payload, it can construct a `VoteMessage` using the
//! signature.
//!
//! We do not need to include such a shred_version for certificates because they
//! are aggregating signatures on the votes.  When verifying a `Certificate`,
//! the verifier would use `Certificate::get_vote_payload()` passing in its own
//! shred_version and if the signature verifies then that implies that the shred
//! versions also match.
//!
//! The actual message that the sender sends over the wire is
//! `VersionedWireConsensusMessage` which can be constructed from a
//! `VoteMessage` or a `Certificate`.  This contains a message version to
//! support upgrades and unlike `ConsensusMessage` the different vote and
//! certificate variants are under a single enum tag to enable [de]coding times.
//! Generally, when a signature on a vote or a certificate does not verify, the
//! receiver would ban the sender.  However, tt is possible that two validators
//! with different shred versions accidentally exchange messages and this should
//! not cause the sender to get banned.  To avoid this,
//! `VersionedWireConsensusMessage` also contains a shred_version that is used
//! to filter out accidental shred_version mismatches.
//!
//! When a receiver processes a `VersionedWireConsensusMessage`, it first
//! converts it to a `DecodedWireConsensusMessage` which performs the
//! shred_version check to avoid accidental mismatches.  This enum contains
//! either a `UnverifiedVoteMessage` or `UnverifiedCertificate`.  Both of these
//! contain all the relevant information to verify them and once verified, the
//! receiver can convert them to `VoteMessage` and `Certificate` respectively.
//!
//! `VoteMessage` verification happens in the bls-sigverify crate and
//! `Certificate` verfication happens in the `bls-cert-verify` crate.

use {
    crate::{
        certificate::{Certificate, CertificateType},
        consensus_message::{Block, ConsensusMessage, VoteMessage},
        vote::Vote,
    },
    serde::{Deserialize, Serialize},
    solana_bls_signatures::Signature as BLSSignature,
    solana_clock::Slot,
    wincode::{
        ReadError, SchemaRead, SchemaReadContext, SchemaWrite, config::Config, io::Reader,
        pod_wrapper,
    },
};

#[cfg(feature = "frozen-abi")]
fn sample_bls_signature(
    rng: &mut (impl solana_frozen_abi::rand::RngCore + ?Sized),
) -> BLSSignature {
    use solana_frozen_abi::stable_abi::StableAbi;
    BLSSignature(<[u8; solana_bls_signatures::BLS_SIGNATURE_AFFINE_SIZE] as StableAbi>::random(rng))
}

// Use `BLSSignature` directly once `BLSSignature` wincode support
// is released in solana-sdk.
pod_wrapper! {
    unsafe struct PodBLSSignature(BLSSignature);
}

#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, StableAbi, StableAbiSample, Serialize)
)]
#[derive(Clone, Debug, Hash, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub(crate) struct WireVoteSignature {
    #[cfg_attr(
        feature = "frozen-abi",
        stable_abi_sample(with = "sample_bls_signature(rng)")
    )]
    #[wincode(with = "PodBLSSignature")]
    pub(crate) signature: BLSSignature,
}

impl From<VoteMessage> for WireVoteSignature {
    fn from(msg: VoteMessage) -> Self {
        Self {
            signature: msg.signature,
        }
    }
}

#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, StableAbi, StableAbiSample, Serialize)
)]
#[derive(Clone, Debug, Hash, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub(crate) struct WireBlockVoteMessage {
    pub(crate) block: Block,
    pub(crate) signature: WireVoteSignature,
}

#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, StableAbi, StableAbiSample, Serialize)
)]
#[derive(Clone, Debug, Hash, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub(crate) struct WireSlotVoteMessage {
    pub(crate) slot: Slot,
    pub(crate) signature: WireVoteSignature,
}

#[cfg_attr(feature = "frozen-abi", derive(AbiExample, StableAbi, StableAbiSample))]
#[derive(Clone, Debug, Hash, PartialEq, Eq, SchemaRead, SchemaWrite, Serialize, Deserialize)]
/// Signature on a wire cert message
pub struct WireCertSignature {
    #[cfg_attr(
        feature = "frozen-abi",
        stable_abi_sample(with = "sample_bls_signature(rng)")
    )]
    #[wincode(with = "PodBLSSignature")]
    /// the aggregate signature
    pub signature: BLSSignature,
    /// bitmap of ranks of validators included in the aggregate.
    pub bitmap: Vec<u8>,
}

impl From<Certificate> for WireCertSignature {
    fn from(cert: Certificate) -> Self {
        Self {
            signature: cert.signature,
            bitmap: cert.bitmap,
        }
    }
}

#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, StableAbi, StableAbiSample, Serialize)
)]
#[derive(Debug, Clone, Hash, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub(crate) struct WireSlotCertMessage {
    pub(crate) slot: Slot,
    pub(crate) signature: WireCertSignature,
}

#[cfg_attr(feature = "frozen-abi", derive(AbiExample, StableAbi, StableAbiSample))]
#[derive(Debug, Clone, Hash, PartialEq, Eq, SchemaRead, SchemaWrite, Serialize, Deserialize)]
/// A wire cert message that holds a block.
pub struct WireBlockCertMessage {
    /// the block the cert is certifying.
    pub block: Block,
    /// the signature of the cert message.
    pub signature: WireCertSignature,
}

#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, StableAbi, StableAbiSample, AbiEnumVisitor, Serialize)
)]
#[derive(Debug, Clone, Hash, PartialEq, Eq, SchemaWrite, SchemaRead)]
#[wincode(tag_encoding = "u8")]
pub(crate) enum WireConsensusMessageKind {
    #[wincode(tag = 1)]
    NotarVote(WireBlockVoteMessage),
    #[wincode(tag = 2)]
    FinalizeVote(WireSlotVoteMessage),
    #[wincode(tag = 3)]
    SkipVote(WireSlotVoteMessage),
    #[wincode(tag = 4)]
    NotarFallbackVote(WireBlockVoteMessage),
    #[wincode(tag = 5)]
    SkipFallbackVote(WireSlotVoteMessage),
    #[wincode(tag = 6)]
    GenesisVote(WireBlockVoteMessage),

    #[wincode(tag = 7)]
    FinalizeCert(WireSlotCertMessage),
    #[wincode(tag = 8)]
    FastFinalizeCert(WireBlockCertMessage),
    #[wincode(tag = 9)]
    NotarCert(WireBlockCertMessage),
    #[wincode(tag = 10)]
    NotarFallbackCert(WireBlockCertMessage),
    #[wincode(tag = 11)]
    SkipCert(WireSlotCertMessage),
    #[wincode(tag = 12)]
    GenesisCert(WireBlockCertMessage),
}

impl WireConsensusMessageKind {
    fn new(msg: ConsensusMessage) -> Self {
        match msg {
            ConsensusMessage::Vote(v) => Self::new_from_vote(v),
            ConsensusMessage::Certificate(c) => Self::new_from_cert(c),
        }
    }

    fn new_from_vote(msg: VoteMessage) -> Self {
        let vote = msg.vote;
        let signature = WireVoteSignature::from(msg);
        match vote {
            Vote::Notarize(v) => Self::NotarVote(WireBlockVoteMessage {
                block: v.block,
                signature,
            }),
            Vote::NotarizeFallback(v) => Self::NotarFallbackVote(WireBlockVoteMessage {
                block: v.block,
                signature,
            }),
            Vote::Finalize(v) => Self::FinalizeVote(WireSlotVoteMessage {
                slot: v.slot,
                signature,
            }),
            Vote::Skip(v) => Self::SkipVote(WireSlotVoteMessage {
                slot: v.slot,
                signature,
            }),
            Vote::SkipFallback(v) => Self::SkipFallbackVote(WireSlotVoteMessage {
                slot: v.slot,
                signature,
            }),
            Vote::Genesis(v) => Self::GenesisVote(WireBlockVoteMessage {
                block: v.block,
                signature,
            }),
        }
    }

    fn new_from_cert(cert: Certificate) -> Self {
        let cert_type = cert.cert_type;
        let signature = WireCertSignature::from(cert);
        match cert_type {
            CertificateType::Finalize(slot) => {
                Self::FinalizeCert(WireSlotCertMessage { slot, signature })
            }
            CertificateType::FinalizeFast(block) => {
                Self::FastFinalizeCert(WireBlockCertMessage { block, signature })
            }
            CertificateType::Notarize(block) => {
                Self::NotarCert(WireBlockCertMessage { block, signature })
            }
            CertificateType::NotarizeFallback(block) => {
                Self::NotarFallbackCert(WireBlockCertMessage { block, signature })
            }
            CertificateType::Skip(slot) => Self::SkipCert(WireSlotCertMessage { slot, signature }),
            CertificateType::Genesis(block) => {
                Self::GenesisCert(WireBlockCertMessage { block, signature })
            }
        }
    }

    /// Returns the slot on the message
    fn slot(&self) -> Slot {
        match self {
            Self::NotarVote(v) | Self::NotarFallbackVote(v) | Self::GenesisVote(v) => v.block.slot,
            Self::FinalizeVote(v) | Self::SkipVote(v) | Self::SkipFallbackVote(v) => v.slot,
            Self::NotarCert(c)
            | Self::GenesisCert(c)
            | Self::FastFinalizeCert(c)
            | Self::NotarFallbackCert(c) => c.block.slot,
            Self::FinalizeCert(c) | Self::SkipCert(c) => c.slot,
        }
    }
}

#[derive(Clone, Copy)]
/// Context wrapper for the expected shred version during deserialization.
pub struct ExpectedShredVersion(pub u16);

#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, StableAbi, StableAbiSample, Serialize, SchemaRead)
)]
#[derive(Debug, Clone, Hash, PartialEq, Eq, SchemaWrite)]
/// First version of a wire consensus message
pub struct WireConsensusMessageV1 {
    pub(crate) kind: WireConsensusMessageKind,
    pub(crate) shred_version: u16,
}

// SAFETY: The custom read_with_context implementation sequentially reads the exact same fields
// as a derived `SchemaRead` implementation would, guaranteeing the safe initialization of `Dst`.
unsafe impl<'de, C: Config> SchemaReadContext<'de, C, ExpectedShredVersion>
    for WireConsensusMessageV1
{
    type Dst = WireConsensusMessageV1;

    fn read_with_context(
        expected: ExpectedShredVersion,
        mut reader: impl Reader<'de>,
        dst: &mut std::mem::MaybeUninit<Self::Dst>,
    ) -> wincode::ReadResult<()> {
        let kind = <WireConsensusMessageKind as SchemaRead<'de, C>>::get(reader.by_ref())?;
        let shred_version = <u16 as SchemaRead<'de, C>>::get(reader.by_ref())?;

        if shred_version != expected.0 {
            return Err(ReadError::Custom("shred version mismatch"));
        }

        dst.write(WireConsensusMessageV1 {
            kind,
            shred_version,
        });

        Ok(())
    }
}

impl WireConsensusMessageV1 {
    fn new(msg: ConsensusMessage, shred_version: u16) -> Self {
        Self {
            kind: WireConsensusMessageKind::new(msg),
            shred_version,
        }
    }

    fn new_from_vote(msg: VoteMessage, shred_version: u16) -> Self {
        Self {
            kind: WireConsensusMessageKind::new_from_vote(msg),
            shred_version,
        }
    }

    fn new_from_cert(cert: Certificate, shred_version: u16) -> Self {
        Self {
            kind: WireConsensusMessageKind::new_from_cert(cert),
            shred_version,
        }
    }

    fn slot(&self) -> Slot {
        self.kind.slot()
    }
}

#[cfg_attr(
    feature = "frozen-abi",
    derive(
        AbiExample,
        AbiEnumVisitor,
        StableAbi,
        StableAbiSample,
        Serialize,
        SchemaRead
    ),
    frozen_abi(
        digest = "DjdpNzNd3eQ569wuRQq7cKAsj791nkjbbhcqWa7TQeKj",
        abi_digest = "ErGjoTr18hn3dvPVA7jFgK5WLwb4jgx7a39Yn8dSzB2K",
        abi_serializer = "wincode",
        test_roundtrip = "eq_and_wire",
    )
)]
#[derive(Debug, Clone, Hash, PartialEq, Eq, SchemaWrite)]
#[wincode(tag_encoding = "u8")]
/// versioned wire format of consensus message
pub enum VersionedWireConsensusMessage {
    /// The first version
    #[wincode(tag = 1)]
    V1(WireConsensusMessageV1),
}

// SAFETY: The custom read_with_context implementation sequentially reads the exact same fields
// as a derived `SchemaRead` implementation would, guaranteeing the safe initialization of `Dst`.
unsafe impl<'de, C: Config> SchemaReadContext<'de, C, ExpectedShredVersion>
    for VersionedWireConsensusMessage
{
    type Dst = VersionedWireConsensusMessage;

    fn read_with_context(
        expected: ExpectedShredVersion,
        mut reader: impl Reader<'de>,
        dst: &mut std::mem::MaybeUninit<Self::Dst>,
    ) -> wincode::ReadResult<()> {
        let tag = reader.take_byte()?;
        match tag {
            1 => {
                let v1 = <WireConsensusMessageV1 as SchemaReadContext<
                    'de,
                    C,
                    ExpectedShredVersion,
                >>::get_with_context(expected, reader)?;
                dst.write(VersionedWireConsensusMessage::V1(v1));
                Ok(())
            }
            _ => Err(ReadError::Custom(
                "unknown tag for VersionedWireConsensusMessage",
            )),
        }
    }
}

impl VersionedWireConsensusMessage {
    /// Deserializes a versioned wire consensus message and verifies the shred version.
    pub fn deserialize_with_expected_shred_version<'de, C: Config>(
        reader: impl Reader<'de>,
        config: C,
        expected_shred_version: u16,
    ) -> wincode::ReadResult<Self> {
        let _ = config;
        <Self as SchemaReadContext<'de, C, _>>::get_with_context(
            ExpectedShredVersion(expected_shred_version),
            reader,
        )
    }

    /// Constructs a new versioned wire consensus message.
    pub fn new(msg: ConsensusMessage, shred_version: u16) -> Self {
        let v1 = WireConsensusMessageV1::new(msg, shred_version);
        Self::V1(v1)
    }

    /// Constructs a new versioned wire consensus message from a vote.
    pub fn new_from_vote(vote: VoteMessage, shred_version: u16) -> Self {
        let v1 = WireConsensusMessageV1::new_from_vote(vote, shred_version);
        Self::V1(v1)
    }

    /// Constructs a new versioned wire consensus message from a cert.
    pub fn new_from_cert(cert: Certificate, shred_version: u16) -> Self {
        let v1 = WireConsensusMessageV1::new_from_cert(cert, shred_version);
        Self::V1(v1)
    }

    /// Returns the slot on the message.
    pub fn slot(&self) -> Slot {
        match self {
            Self::V1(v1) => v1.slot(),
        }
    }
}

#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample, AbiEnumVisitor, StableAbi, StableAbiSample, Serialize),
    frozen_abi(
        digest = "AKMt6bqYRf1xh7tWg4eAgG7jNN1qtUvNVPb5XZn9vjtV",
        abi_digest = "2aBMTuPyDgGSYeYX1aBbXURgA4qqr92Eh9yiTeHX6qZq",
        abi_serializer = "wincode",
        test_roundtrip = "eq_and_wire",
    )
)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, SchemaWrite, SchemaRead)]
#[wincode(tag_encoding = "u8")]
/// Vote payload that must be signed
pub enum VotePayloadToSign {
    #[wincode(tag = 1)]
    /// notar vote
    Notar {
        /// block
        block: Block,
        /// shred version
        shred_version: u16,
    },
    #[wincode(tag = 2)]
    /// finalize vote
    Finalize {
        /// slot
        slot: Slot,
        /// shred version
        shred_version: u16,
    },
    #[wincode(tag = 3)]
    /// skip vote
    Skip {
        /// slot
        slot: Slot,
        /// shred version
        shred_version: u16,
    },
    #[wincode(tag = 4)]
    /// notar fallback vote
    NotarFallback {
        /// block
        block: Block,
        /// shred version
        shred_version: u16,
    },
    #[wincode(tag = 5)]
    /// skip fallback vote
    SkipFallback {
        /// slot
        slot: Slot,
        /// shred version
        shred_version: u16,
    },
    #[wincode(tag = 6)]
    /// genesis vote
    Genesis {
        /// block
        block: Block,
        /// shred version
        shred_version: u16,
    },
}

impl VotePayloadToSign {
    /// Converts a `Vote` into a `VotePayloadToSign`
    pub fn new_from_vote(vote: Vote, shred_version: u16) -> Self {
        match vote {
            Vote::Notarize(v) => Self::Notar {
                block: v.block,
                shred_version,
            },
            Vote::NotarizeFallback(v) => Self::NotarFallback {
                block: v.block,
                shred_version,
            },
            Vote::Genesis(v) => Self::Genesis {
                block: v.block,
                shred_version,
            },
            Vote::Finalize(v) => Self::Finalize {
                slot: v.slot,
                shred_version,
            },
            Vote::Skip(v) => Self::Skip {
                slot: v.slot,
                shred_version,
            },
            Vote::SkipFallback(v) => Self::SkipFallback {
                slot: v.slot,
                shred_version,
            },
        }
    }

    /// Returns the slot the vote is for.
    pub fn slot(&self) -> Slot {
        match self {
            Self::Notar { block, .. }
            | Self::NotarFallback { block, .. }
            | Self::Genesis { block, .. } => block.slot,
            Self::Finalize { slot, .. }
            | Self::Skip { slot, .. }
            | Self::SkipFallback { slot, .. } => *slot,
        }
    }
}

impl From<VotePayloadToSign> for Vote {
    /// Converts a `VotePayloadToSign` back into a `Vote`, dropping the shred version.
    fn from(vote_payload: VotePayloadToSign) -> Self {
        match vote_payload {
            VotePayloadToSign::Notar { block, .. } => Self::new_notarization_vote(block),
            VotePayloadToSign::Finalize { slot, .. } => Self::new_finalization_vote(slot),
            VotePayloadToSign::Skip { slot, .. } => Self::new_skip_vote(slot),
            VotePayloadToSign::NotarFallback { block, .. } => {
                Self::new_notarization_fallback_vote(block)
            }
            VotePayloadToSign::SkipFallback { slot, .. } => Self::new_skip_fallback_vote(slot),
            VotePayloadToSign::Genesis { block, .. } => Self::new_genesis_vote(block),
        }
    }
}

/// Returns the appropriate vote payload to sign.
pub fn get_vote_payload_to_sign(vote: Vote, shred_version: u16) -> Vec<u8> {
    let vote_to_sign = VotePayloadToSign::new_from_vote(vote, shred_version);
    wincode::serialize(&vote_to_sign).unwrap()
}
