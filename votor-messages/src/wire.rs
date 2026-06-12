//! wire types
use {
    crate::{
        certificate::{Certificate, CertificateType},
        consensus_message::{Block, ConsensusMessage, VoteMessage},
        vote::Vote,
    },
    serde::{Deserialize, Serialize},
    solana_bls_signatures::Signature as BLSSignature,
    solana_clock::Slot,
    wincode::{SchemaRead, SchemaWrite, pod_wrapper},
};

// Use `BLSSignature` directly once `BLSSignature` wincode support
// is released in solana-sdk.
pod_wrapper! {
    unsafe struct PodBLSSignature(BLSSignature);
}

#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireNotarVote {
    block: Block,
    shred_version: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireNotarFallbackVote {
    block: Block,
    shred_version: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireGenesisVote {
    block: Block,
    shred_version: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireFinalizeVote {
    slot: Slot,
    shred_version: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireSkipVote {
    slot: Slot,
    shred_version: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireSkipFallbackVote {
    slot: Slot,
    shred_version: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireVoteSignature {
    #[wincode(with = "PodBLSSignature")]
    signature: BLSSignature,
    rank: u16,
}

impl From<VoteMessage> for WireVoteSignature {
    fn from(msg: VoteMessage) -> Self {
        Self {
            signature: msg.signature,
            rank: msg.rank,
        }
    }
}

/// wire format of a notar vote message
#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
pub struct WireNotarVoteMessage {
    vote: WireNotarVote,
    signature: WireVoteSignature,
}

impl From<WireNotarVoteMessage> for VoteMessage {
    fn from(msg: WireNotarVoteMessage) -> Self {
        let vote = Vote::new_notarization_vote(msg.vote.block);
        Self {
            vote,
            signature: msg.signature.signature,
            rank: msg.signature.rank,
        }
    }
}

/// wire format of a notar fallback vote message
#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
pub struct WireNotarFallbackVoteMessage {
    vote: WireNotarFallbackVote,
    signature: WireVoteSignature,
}

impl From<WireNotarFallbackVoteMessage> for VoteMessage {
    fn from(msg: WireNotarFallbackVoteMessage) -> Self {
        let vote = Vote::new_notarization_fallback_vote(msg.vote.block);
        Self {
            vote,
            signature: msg.signature.signature,
            rank: msg.signature.rank,
        }
    }
}

/// wire format of a genesis vote message
#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
pub struct WireGenesisVoteMessage {
    vote: WireGenesisVote,
    signature: WireVoteSignature,
}

impl From<WireGenesisVoteMessage> for VoteMessage {
    fn from(msg: WireGenesisVoteMessage) -> Self {
        let vote = Vote::new_genesis_vote(msg.vote.block);
        Self {
            vote,
            signature: msg.signature.signature,
            rank: msg.signature.rank,
        }
    }
}

/// wire format of a finalize vote message
#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
pub struct WireFinalizeVoteMessage {
    vote: WireFinalizeVote,
    signature: WireVoteSignature,
}

impl From<WireFinalizeVoteMessage> for VoteMessage {
    fn from(msg: WireFinalizeVoteMessage) -> Self {
        let vote = Vote::new_finalization_vote(msg.vote.slot);
        Self {
            vote,
            signature: msg.signature.signature,
            rank: msg.signature.rank,
        }
    }
}

/// wire format of a skip vote message
#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
pub struct WireSkipVoteMessage {
    vote: WireSkipVote,
    signature: WireVoteSignature,
}

impl From<WireSkipVoteMessage> for VoteMessage {
    fn from(msg: WireSkipVoteMessage) -> Self {
        let vote = Vote::new_skip_vote(msg.vote.slot);
        Self {
            vote,
            signature: msg.signature.signature,
            rank: msg.signature.rank,
        }
    }
}

/// wire format of a skip fallback vote message
#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
pub struct WireSkipFallbackVoteMessage {
    vote: WireSkipFallbackVote,
    signature: WireVoteSignature,
}

impl From<WireSkipFallbackVoteMessage> for VoteMessage {
    fn from(msg: WireSkipFallbackVoteMessage) -> Self {
        let vote = Vote::new_skip_fallback_vote(msg.vote.slot);
        Self {
            vote,
            signature: msg.signature.signature,
            rank: msg.signature.rank,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireFinalizeCert {
    slot: Slot,
    shred_version: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireSkipCert {
    slot: Slot,
    shred_version: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireFastFinalizeCert {
    block: Block,
    shred_version: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireNotarCert {
    block: Block,
    shred_version: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireNotarFallbackCert {
    block: Block,
    shred_version: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireGenesisCert {
    block: Block,
    shred_version: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireCertSignature {
    #[wincode(with = "PodBLSSignature")]
    signature: BLSSignature,
    bitmap: Vec<u8>,
}

impl From<Certificate> for WireCertSignature {
    fn from(cert: Certificate) -> Self {
        Self {
            signature: cert.signature,
            bitmap: cert.bitmap,
        }
    }
}

/// wire format of a finalize cert message
#[derive(Debug, Clone, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
pub struct WireFinalizeCertMessage {
    cert: WireFinalizeCert,
    signature: WireCertSignature,
}

impl From<WireFinalizeCertMessage> for Certificate {
    fn from(cert: WireFinalizeCertMessage) -> Self {
        let cert_type = CertificateType::Finalize(cert.cert.slot);
        Self {
            cert_type,
            signature: cert.signature.signature,
            bitmap: cert.signature.bitmap,
        }
    }
}

/// wire format of a skip cert message
#[derive(Debug, Clone, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
pub struct WireSkipCertMessage {
    cert: WireSkipCert,
    signature: WireCertSignature,
}

impl From<WireSkipCertMessage> for Certificate {
    fn from(cert: WireSkipCertMessage) -> Self {
        let cert_type = CertificateType::Skip(cert.cert.slot);
        Self {
            cert_type,
            signature: cert.signature.signature,
            bitmap: cert.signature.bitmap,
        }
    }
}

/// wire format of a fast finalize cert message
#[derive(Debug, Clone, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
pub struct WireFastFinalizeCertMessage {
    cert: WireFastFinalizeCert,
    signature: WireCertSignature,
}

impl From<WireFastFinalizeCertMessage> for Certificate {
    fn from(cert: WireFastFinalizeCertMessage) -> Self {
        let cert_type = CertificateType::FinalizeFast(cert.cert.block);
        Self {
            cert_type,
            signature: cert.signature.signature,
            bitmap: cert.signature.bitmap,
        }
    }
}

/// wire format of a notar cert message
#[derive(Debug, Clone, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
pub struct WireNotarCertMessage {
    cert: WireNotarCert,
    signature: WireCertSignature,
}

impl From<WireNotarCertMessage> for Certificate {
    fn from(cert: WireNotarCertMessage) -> Self {
        let cert_type = CertificateType::Notarize(cert.cert.block);
        Self {
            cert_type,
            signature: cert.signature.signature,
            bitmap: cert.signature.bitmap,
        }
    }
}

/// wire format of a notar fallback cert message
#[derive(Debug, Clone, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
pub struct WireNotarFallbackCertMessage {
    cert: WireNotarFallbackCert,
    signature: WireCertSignature,
}

impl From<WireNotarFallbackCertMessage> for Certificate {
    fn from(cert: WireNotarFallbackCertMessage) -> Self {
        let cert_type = CertificateType::NotarizeFallback(cert.cert.block);
        Self {
            cert_type,
            signature: cert.signature.signature,
            bitmap: cert.signature.bitmap,
        }
    }
}

/// wire format of a genesis cert message
#[derive(Debug, Clone, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
pub struct WireGenesisCertMessage {
    cert: WireGenesisCert,
    signature: WireCertSignature,
}

impl From<WireGenesisCertMessage> for Certificate {
    fn from(cert: WireGenesisCertMessage) -> Self {
        let cert_type = CertificateType::Genesis(cert.cert.block);
        Self {
            cert_type,
            signature: cert.signature.signature,
            bitmap: cert.signature.bitmap,
        }
    }
}

/// first version of the wire format of consensus message.
#[derive(Debug, Clone, PartialEq, Eq, SchemaWrite, SchemaRead, Deserialize, Serialize)]
#[wincode(tag_encoding = "u8")]
pub enum WireConsensusMessageV1 {
    /// notar vote variant
    #[wincode(tag = 1)]
    NotarVote(WireNotarVoteMessage),
    /// finalize vote variant
    #[wincode(tag = 2)]
    FinalizeVote(WireFinalizeVoteMessage),
    /// skip vote variant
    #[wincode(tag = 3)]
    SkipVote(WireSkipVoteMessage),
    /// notar fallback vote variant
    #[wincode(tag = 4)]
    NotarFallbackVote(WireNotarFallbackVoteMessage),
    /// skip fallback vote variant
    #[wincode(tag = 5)]
    SkipFallbackVote(WireSkipFallbackVoteMessage),
    /// genesis vote variant
    #[wincode(tag = 6)]
    GenesisVote(WireGenesisVoteMessage),

    /// finalize cert variant
    #[wincode(tag = 7)]
    FinalizeCert(WireFinalizeCertMessage),
    /// fast finalize cert variant
    #[wincode(tag = 8)]
    FastFinalizeCert(WireFastFinalizeCertMessage),
    /// notar cert variant
    #[wincode(tag = 9)]
    NotarCert(WireNotarCertMessage),
    /// notar fallback cert variant
    #[wincode(tag = 10)]
    NotarFallbackCert(WireNotarFallbackCertMessage),
    /// skip cert variant
    #[wincode(tag = 11)]
    SkipCert(WireSkipCertMessage),
    /// genesis cert variant
    #[wincode(tag = 12)]
    GenesisCert(WireGenesisCertMessage),
}

impl WireConsensusMessageV1 {
    /// Constructs a new version 1 wire consensus message.
    pub fn new(msg: ConsensusMessage, shred_version: u16) -> Self {
        match msg {
            ConsensusMessage::Vote(v) => Self::new_from_vote(v, shred_version),
            ConsensusMessage::Certificate(c) => Self::new_from_cert(c, shred_version),
        }
    }

    /// Constructs a new version 1 wire consensus message from a vote.
    pub fn new_from_vote(msg: VoteMessage, shred_version: u16) -> Self {
        let vote = msg.vote;
        let signature = WireVoteSignature::from(msg);
        match vote {
            Vote::Notarize(n) => Self::NotarVote(WireNotarVoteMessage {
                vote: WireNotarVote {
                    block: n.block,
                    shred_version,
                },
                signature,
            }),
            Vote::NotarizeFallback(n) => Self::NotarFallbackVote(WireNotarFallbackVoteMessage {
                vote: WireNotarFallbackVote {
                    block: n.block,
                    shred_version,
                },
                signature,
            }),
            Vote::Finalize(n) => Self::FinalizeVote(WireFinalizeVoteMessage {
                vote: WireFinalizeVote {
                    slot: n.slot,
                    shred_version,
                },
                signature,
            }),
            Vote::Skip(n) => Self::SkipVote(WireSkipVoteMessage {
                vote: WireSkipVote {
                    slot: n.slot,
                    shred_version,
                },
                signature,
            }),
            Vote::SkipFallback(n) => Self::SkipFallbackVote(WireSkipFallbackVoteMessage {
                vote: WireSkipFallbackVote {
                    slot: n.slot,
                    shred_version,
                },
                signature,
            }),
            Vote::Genesis(n) => Self::GenesisVote(WireGenesisVoteMessage {
                vote: WireGenesisVote {
                    block: n.block,
                    shred_version,
                },
                signature,
            }),
        }
    }

    /// Constructs a new version 1 wire consensus message from a cert.
    pub fn new_from_cert(cert: Certificate, shred_version: u16) -> Self {
        let cert_type = cert.cert_type;
        let signature = WireCertSignature::from(cert);
        match cert_type {
            CertificateType::Finalize(slot) => Self::FinalizeCert(WireFinalizeCertMessage {
                cert: WireFinalizeCert {
                    slot,
                    shred_version,
                },
                signature,
            }),
            CertificateType::FinalizeFast(block) => {
                Self::FastFinalizeCert(WireFastFinalizeCertMessage {
                    cert: WireFastFinalizeCert {
                        block,
                        shred_version,
                    },
                    signature,
                })
            }
            CertificateType::Notarize(block) => Self::NotarCert(WireNotarCertMessage {
                cert: WireNotarCert {
                    block,
                    shred_version,
                },
                signature,
            }),
            CertificateType::NotarizeFallback(block) => {
                Self::NotarFallbackCert(WireNotarFallbackCertMessage {
                    cert: WireNotarFallbackCert {
                        block,
                        shred_version,
                    },
                    signature,
                })
            }
            CertificateType::Skip(slot) => Self::SkipCert(WireSkipCertMessage {
                cert: WireSkipCert {
                    slot,
                    shred_version,
                },
                signature,
            }),
            CertificateType::Genesis(block) => Self::GenesisCert(WireGenesisCertMessage {
                cert: WireGenesisCert {
                    block,
                    shred_version,
                },
                signature,
            }),
        }
    }

    /// returns the shred version
    pub fn shred_version(&self) -> u16 {
        match self {
            Self::NotarVote(v) => v.vote.shred_version,
            Self::NotarFallbackVote(v) => v.vote.shred_version,
            Self::FinalizeVote(v) => v.vote.shred_version,
            Self::SkipVote(v) => v.vote.shred_version,
            Self::SkipFallbackVote(v) => v.vote.shred_version,
            Self::GenesisVote(v) => v.vote.shred_version,
            Self::NotarCert(c) => c.cert.shred_version,
            Self::SkipCert(c) => c.cert.shred_version,
            Self::FinalizeCert(c) => c.cert.shred_version,
            Self::FastFinalizeCert(c) => c.cert.shred_version,
            Self::NotarFallbackCert(c) => c.cert.shred_version,
            Self::GenesisCert(c) => c.cert.shred_version,
        }
    }
}

impl From<WireConsensusMessageV1> for ConsensusMessage {
    fn from(msg: WireConsensusMessageV1) -> Self {
        match msg {
            WireConsensusMessageV1::NotarVote(v) => Self::Vote(VoteMessage::from(v)),
            WireConsensusMessageV1::FinalizeVote(v) => Self::Vote(VoteMessage::from(v)),
            WireConsensusMessageV1::SkipVote(v) => Self::Vote(VoteMessage::from(v)),
            WireConsensusMessageV1::NotarFallbackVote(v) => Self::Vote(VoteMessage::from(v)),
            WireConsensusMessageV1::SkipFallbackVote(v) => Self::Vote(VoteMessage::from(v)),
            WireConsensusMessageV1::GenesisVote(v) => Self::Vote(VoteMessage::from(v)),
            WireConsensusMessageV1::FinalizeCert(c) => Self::Certificate(Certificate::from(c)),
            WireConsensusMessageV1::FastFinalizeCert(c) => Self::Certificate(Certificate::from(c)),
            WireConsensusMessageV1::NotarCert(c) => Self::Certificate(Certificate::from(c)),
            WireConsensusMessageV1::NotarFallbackCert(c) => Self::Certificate(Certificate::from(c)),
            WireConsensusMessageV1::SkipCert(c) => Self::Certificate(Certificate::from(c)),
            WireConsensusMessageV1::GenesisCert(c) => Self::Certificate(Certificate::from(c)),
        }
    }
}

/// versioned wire format of consensus message
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead)]
#[wincode(tag_encoding = "u8")]
pub enum VersionedWireConsensusMessage {
    /// The first version
    #[wincode(tag = 1)]
    V1(WireConsensusMessageV1),
}

impl VersionedWireConsensusMessage {
    /// Constructs a new versionsed wire consensus message.
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

    /// returns the shred version
    pub fn shred_version(&self) -> u16 {
        match self {
            Self::V1(v1) => v1.shred_version(),
        }
    }
}

impl From<VersionedWireConsensusMessage> for ConsensusMessage {
    fn from(msg: VersionedWireConsensusMessage) -> Self {
        match msg {
            VersionedWireConsensusMessage::V1(msg) => Self::from(msg),
        }
    }
}
