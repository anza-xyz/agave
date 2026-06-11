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

/// Wire format of a notar vote.
#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireNotarVote {
    block: Block,
}

/// Wire format of a notar fallback vote.
#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireNotarFallbackVote {
    block: Block,
}

/// Wire format of a finalize vote.
#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireFinalizeVote {
    slot: Slot,
}

/// Wire format of a skip vote.
#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireSkipVote {
    slot: Slot,
}

/// Wire format of a skip fallback vote.
#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireSkipFallbackVote {
    slot: Slot,
}

/// Wire format of a genesis vote.
#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireGenesisVote {
    block: Block,
}

#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct VoteSignature {
    #[wincode(with = "PodBLSSignature")]
    signature: BLSSignature,
    rank: u16,
}

impl From<VoteMessage> for VoteSignature {
    fn from(msg: VoteMessage) -> Self {
        Self {
            signature: msg.signature,
            rank: msg.rank,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireNotarVoteMessage {
    vote: WireNotarVote,
    signature: VoteSignature,
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

#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireNotarFallbackVoteMessage {
    vote: WireNotarFallbackVote,
    signature: VoteSignature,
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

#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireGenesisVoteMessage {
    vote: WireGenesisVote,
    signature: VoteSignature,
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

#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireFinalizeVoteMessage {
    vote: WireFinalizeVote,
    signature: VoteSignature,
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

#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireSkipVoteMessage {
    vote: WireSkipVote,
    signature: VoteSignature,
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

#[derive(Clone, Debug, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireSkipFallbackVoteMessage {
    vote: WireSkipFallbackVote,
    signature: VoteSignature,
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
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireSkipCert {
    slot: Slot,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireFastFinalizeCert {
    block: Block,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireNotarCert {
    block: Block,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireNotarFallbackCert {
    block: Block,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireGenesisCert {
    block: Block,
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

#[derive(Debug, Clone, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireFinalizeCertMessage {
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

#[derive(Debug, Clone, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireSkipCertMessage {
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

#[derive(Debug, Clone, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireFastFinalizeCertMessage {
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

#[derive(Debug, Clone, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireNotarCertMessage {
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

#[derive(Debug, Clone, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireNotarFallbackCertMessage {
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

#[derive(Debug, Clone, PartialEq, Eq, SchemaRead, SchemaWrite, Deserialize, Serialize)]
struct WireGenesisCertMessage {
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

#[derive(Debug, Clone, PartialEq, Eq, SchemaWrite, SchemaRead, Deserialize, Serialize)]
enum WireConsensusMessageKind {
    NotarVote(WireNotarVoteMessage),
    FinalizeVote(WireFinalizeVoteMessage),
    SkipVote(WireSkipVoteMessage),
    NotarFallbackVote(WireNotarFallbackVoteMessage),
    SkipFallbackVote(WireSkipFallbackVoteMessage),
    GenesisVote(WireGenesisVoteMessage),

    FinalizeCert(WireFinalizeCertMessage),
    FastFinalizeCert(WireFastFinalizeCertMessage),
    NotarCert(WireNotarCertMessage),
    NotarFallbackCert(WireNotarFallbackCertMessage),
    SkipCert(WireSkipCertMessage),
    GenesisCert(WireGenesisCertMessage),
}

/// first version of the wire format of consensus message.
#[derive(Debug, Clone, PartialEq, Eq, SchemaWrite, SchemaRead, Deserialize, Serialize)]
pub struct WireConsensusMessageV1 {
    kind: WireConsensusMessageKind,
    /// The shred version for this run of the cluster
    pub shred_version: u16,
}

impl WireConsensusMessageV1 {
    /// Constructs a new version 1 wire consensus message.
    pub fn new(msg: ConsensusMessage, shred_version: u16) -> Self {
        let kind = WireConsensusMessageKind::from(msg);
        Self {
            kind,
            shred_version,
        }
    }

    /// Constructs a new version 1 wire consensus message from a vote.
    pub fn new_from_vote(vote: VoteMessage, shred_version: u16) -> Self {
        let kind = WireConsensusMessageKind::from(vote);
        Self {
            kind,
            shred_version,
        }
    }

    /// Constructs a new version 1 wire consensus message from a cert.
    pub fn new_from_cert(cert: Certificate, shred_version: u16) -> Self {
        let kind = WireConsensusMessageKind::from(cert);
        Self {
            kind,
            shred_version,
        }
    }
}

impl From<WireConsensusMessageV1> for ConsensusMessage {
    fn from(msg: WireConsensusMessageV1) -> Self {
        match msg.kind {
            WireConsensusMessageKind::NotarVote(v) => Self::Vote(VoteMessage::from(v)),
            WireConsensusMessageKind::FinalizeVote(v) => Self::Vote(VoteMessage::from(v)),
            WireConsensusMessageKind::SkipVote(v) => Self::Vote(VoteMessage::from(v)),
            WireConsensusMessageKind::NotarFallbackVote(v) => Self::Vote(VoteMessage::from(v)),
            WireConsensusMessageKind::SkipFallbackVote(v) => Self::Vote(VoteMessage::from(v)),
            WireConsensusMessageKind::GenesisVote(v) => Self::Vote(VoteMessage::from(v)),
            WireConsensusMessageKind::FinalizeCert(c) => Self::Certificate(Certificate::from(c)),
            WireConsensusMessageKind::FastFinalizeCert(c) => {
                Self::Certificate(Certificate::from(c))
            }
            WireConsensusMessageKind::NotarCert(c) => Self::Certificate(Certificate::from(c)),
            WireConsensusMessageKind::NotarFallbackCert(c) => {
                Self::Certificate(Certificate::from(c))
            }
            WireConsensusMessageKind::SkipCert(c) => Self::Certificate(Certificate::from(c)),
            WireConsensusMessageKind::GenesisCert(c) => Self::Certificate(Certificate::from(c)),
        }
    }
}

/// versioned wire format of consensus message
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub enum VersionedWireConsensusMessage {
    /// The first version
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
}

impl From<VersionedWireConsensusMessage> for ConsensusMessage {
    fn from(msg: VersionedWireConsensusMessage) -> Self {
        match msg {
            VersionedWireConsensusMessage::V1(msg) => Self::from(msg),
        }
    }
}

impl From<VoteMessage> for WireConsensusMessageKind {
    fn from(msg: VoteMessage) -> Self {
        match msg.vote {
            Vote::Notarize(v) => WireConsensusMessageKind::NotarVote(WireNotarVoteMessage {
                vote: WireNotarVote { block: v.block },
                signature: msg.into(),
            }),
            Vote::NotarizeFallback(v) => {
                WireConsensusMessageKind::NotarFallbackVote(WireNotarFallbackVoteMessage {
                    vote: WireNotarFallbackVote { block: v.block },
                    signature: msg.into(),
                })
            }
            Vote::Finalize(v) => WireConsensusMessageKind::FinalizeVote(WireFinalizeVoteMessage {
                vote: WireFinalizeVote { slot: v.slot },
                signature: msg.into(),
            }),
            Vote::Skip(v) => WireConsensusMessageKind::SkipVote(WireSkipVoteMessage {
                vote: WireSkipVote { slot: v.slot },
                signature: msg.into(),
            }),
            Vote::SkipFallback(v) => {
                WireConsensusMessageKind::SkipFallbackVote(WireSkipFallbackVoteMessage {
                    vote: WireSkipFallbackVote { slot: v.slot },
                    signature: msg.into(),
                })
            }
            Vote::Genesis(v) => WireConsensusMessageKind::GenesisVote(WireGenesisVoteMessage {
                vote: WireGenesisVote { block: v.block },
                signature: msg.into(),
            }),
        }
    }
}

impl From<Certificate> for WireConsensusMessageKind {
    fn from(cert: Certificate) -> Self {
        match cert.cert_type {
            CertificateType::Finalize(slot) => {
                WireConsensusMessageKind::FinalizeCert(WireFinalizeCertMessage {
                    cert: WireFinalizeCert { slot },
                    signature: cert.into(),
                })
            }
            CertificateType::FinalizeFast(block) => {
                WireConsensusMessageKind::FastFinalizeCert(WireFastFinalizeCertMessage {
                    cert: WireFastFinalizeCert { block },
                    signature: cert.into(),
                })
            }
            CertificateType::Notarize(block) => {
                WireConsensusMessageKind::NotarCert(WireNotarCertMessage {
                    cert: WireNotarCert { block },
                    signature: cert.into(),
                })
            }
            CertificateType::NotarizeFallback(block) => {
                WireConsensusMessageKind::NotarFallbackCert(WireNotarFallbackCertMessage {
                    cert: WireNotarFallbackCert { block },
                    signature: cert.into(),
                })
            }
            CertificateType::Skip(slot) => {
                WireConsensusMessageKind::SkipCert(WireSkipCertMessage {
                    cert: WireSkipCert { slot },
                    signature: cert.into(),
                })
            }
            CertificateType::Genesis(block) => {
                WireConsensusMessageKind::GenesisCert(WireGenesisCertMessage {
                    cert: WireGenesisCert { block },
                    signature: cert.into(),
                })
            }
        }
    }
}

impl From<ConsensusMessage> for WireConsensusMessageKind {
    fn from(msg: ConsensusMessage) -> Self {
        match msg {
            ConsensusMessage::Vote(v) => Self::from(v),
            ConsensusMessage::Certificate(c) => Self::from(c),
        }
    }
}
