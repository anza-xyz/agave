//! Defines unverified vote message and related types

use {
    crate::{
        certificate::CertificateType,
        vote::Vote,
        wire::{VersionedWireConsensusMessage, WireConsensusMessageKind},
    },
    solana_bls_signatures::Signature as BLSSignature,
};

/// An unverified vote message.
#[derive(Clone, Debug)]
pub struct UnverifiedVoteMessage {
    /// The vote payload that is signed.
    pub vote: Vote,
    /// The signature
    pub signature: BLSSignature,
    /// the shred version
    pub shred_version: u16,
}

/// an unverified certificate
#[derive(Clone, Debug)]
pub struct UnverifiedCertificate {
    /// The certificate type.
    pub cert_type: CertificateType,
    /// The aggregate signature.
    pub signature: BLSSignature,
    /// A rank bitmap for validators' signatures included in the aggregate.
    /// See solana-signer-store for encoding format.
    pub bitmap: Vec<u8>,
    /// the shred version
    pub shred_version: u16,
}

/// Output of decoding a wire consensus message into unverified vote or certificate.
pub enum DecodedWireConsensusMessage {
    /// Decoded to a vote
    Vote(UnverifiedVoteMessage),
    /// Decoded to a certificate
    Certificate(UnverifiedCertificate),
}

impl DecodedWireConsensusMessage {
    /// Decodes a wire consensus message.
    pub fn new(msg: VersionedWireConsensusMessage) -> Self {
        let VersionedWireConsensusMessage::V1(msg) = msg;
        match msg.kind {
            WireConsensusMessageKind::NotarVote(v) => Self::Vote(UnverifiedVoteMessage {
                vote: Vote::new_notarization_vote(v.block),
                signature: v.signature.signature,
                shred_version: msg.shred_version,
            }),
            WireConsensusMessageKind::NotarFallbackVote(v) => Self::Vote(UnverifiedVoteMessage {
                vote: Vote::new_notarization_fallback_vote(v.block),
                signature: v.signature.signature,
                shred_version: msg.shred_version,
            }),
            WireConsensusMessageKind::FinalizeVote(v) => Self::Vote(UnverifiedVoteMessage {
                vote: Vote::new_finalization_vote(v.slot),
                signature: v.signature.signature,
                shred_version: msg.shred_version,
            }),
            WireConsensusMessageKind::SkipVote(v) => Self::Vote(UnverifiedVoteMessage {
                vote: Vote::new_skip_vote(v.slot),
                signature: v.signature.signature,
                shred_version: msg.shred_version,
            }),
            WireConsensusMessageKind::SkipFallbackVote(v) => Self::Vote(UnverifiedVoteMessage {
                vote: Vote::new_skip_fallback_vote(v.slot),
                signature: v.signature.signature,
                shred_version: msg.shred_version,
            }),
            WireConsensusMessageKind::GenesisVote(v) => Self::Vote(UnverifiedVoteMessage {
                vote: Vote::new_genesis_vote(v.block),
                signature: v.signature.signature,
                shred_version: msg.shred_version,
            }),

            WireConsensusMessageKind::NotarCert(c) => {
                let cert_type = CertificateType::Notarize(c.block);
                Self::Certificate(UnverifiedCertificate {
                    cert_type,
                    signature: c.signature.signature,
                    bitmap: c.signature.bitmap,
                    shred_version: msg.shred_version,
                })
            }
            WireConsensusMessageKind::FinalizeCert(c) => {
                let cert_type = CertificateType::Finalize(c.slot);
                Self::Certificate(UnverifiedCertificate {
                    cert_type,
                    signature: c.signature.signature,
                    bitmap: c.signature.bitmap,
                    shred_version: msg.shred_version,
                })
            }
            WireConsensusMessageKind::FastFinalizeCert(c) => {
                let cert_type = CertificateType::FinalizeFast(c.block);
                Self::Certificate(UnverifiedCertificate {
                    cert_type,
                    signature: c.signature.signature,
                    bitmap: c.signature.bitmap,
                    shred_version: msg.shred_version,
                })
            }
            WireConsensusMessageKind::NotarFallbackCert(c) => {
                let cert_type = CertificateType::NotarizeFallback(c.block);
                Self::Certificate(UnverifiedCertificate {
                    cert_type,
                    signature: c.signature.signature,
                    bitmap: c.signature.bitmap,
                    shred_version: msg.shred_version,
                })
            }
            WireConsensusMessageKind::SkipCert(c) => {
                let cert_type = CertificateType::Skip(c.slot);
                Self::Certificate(UnverifiedCertificate {
                    cert_type,
                    signature: c.signature.signature,
                    bitmap: c.signature.bitmap,
                    shred_version: msg.shred_version,
                })
            }
            WireConsensusMessageKind::GenesisCert(c) => {
                let cert_type = CertificateType::Genesis(c.block);
                Self::Certificate(UnverifiedCertificate {
                    cert_type,
                    signature: c.signature.signature,
                    bitmap: c.signature.bitmap,
                    shred_version: msg.shred_version,
                })
            }
        }
    }

    /// returns the shred version
    pub fn shred_version(&self) -> u16 {
        match self {
            Self::Vote(v) => v.shred_version,
            Self::Certificate(c) => c.shred_version,
        }
    }
}
