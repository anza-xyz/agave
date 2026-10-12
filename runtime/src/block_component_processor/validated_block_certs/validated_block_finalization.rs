//! Validated block finalization certificates.
//!
//! This module provides [`ValidatedBlockFinalizationCert`], a type that represents
//! a validated proof that a block has been finalized. The type can only be constructed
//! through validation, ensuring type-safe guarantees about certificate validity.

use {
    crate::bank::Bank,
    agave_votor_messages::{
        certificate::{Certificate, CertificateType, FastFinalizeCert, FinalizeCert, NotarCert},
        consensus_message::Block,
        finalized_slot::FinalizedSlot,
    },
    itertools::Either,
    solana_clock::Slot,
    solana_entry::block_component::{BlockFinalizationCert, VotesAggregate},
    solana_pubkey::Pubkey,
    solana_signer_store::{DecodeError, Decoded, decode},
    std::collections::HashSet,
    thiserror::Error,
};

/// Error type for block finalization certificate validation.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum BlockFinalizationCertError {
    /// Missing epoch stakes for slot
    #[error("Missing epoch stakes for {0:?}")]
    MissingEpochStakes(CertificateType),

    /// Base 3 finalization certificate
    #[error("Finalization certificate was base 3 for {0:?}")]
    Base3Finalization(CertificateType),

    /// Decoding error
    #[error("Unable to decode certificate {0:?}: {1:?}")]
    DecodeError(CertificateType, DecodeError),
}

/// There are two ways a block can be finalized:
/// - `Finalize`: Slow finalization with both a Finalize certificate and a Notarize certificate
/// - `FastFinalize`: Fast finalization with a single FinalizeFast certificate
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
enum ValidatedBlockFinalizationCertKind {
    /// Slow finalization: requires both a Finalize cert and a Notarize cert for the same slot
    Finalize {
        /// The finalize certificate
        finalize_cert: FinalizeCert,
        /// The notarize certificate
        notarize_cert: NotarCert,
    },
    /// Fast finalization: a single FastFinalize certificate
    FastFinalize(FastFinalizeCert),
}

/// A validated proof that a block has been finalized.
///
/// This type can only be constructed through validation methods, providing
/// compile-time guarantees that the contained certificates are valid.
#[derive(Clone, Debug)]
pub struct ValidatedBlockFinalizationCert {
    kind: ValidatedBlockFinalizationCertKind,
    signers: HashSet<Pubkey>,
}

impl ValidatedBlockFinalizationCert {
    pub(super) fn try_from_validated(
        certs: Either<FastFinalizeCert, (NotarCert, FinalizeCert)>,
        signers: HashSet<Pubkey>,
    ) -> Self {
        match certs {
            Either::Left(fast) => Self {
                kind: ValidatedBlockFinalizationCertKind::FastFinalize(fast),
                signers,
            },
            Either::Right((notarize_cert, finalize_cert)) => Self {
                kind: ValidatedBlockFinalizationCertKind::Finalize {
                    finalize_cert,
                    notarize_cert,
                },
                signers,
            },
        }
    }

    /// Creates a validated block finalization certificate from already-validated certificates.
    ///
    /// This is intended for use by the consensus pool, where certificates have already been
    /// validated by the bls sigverifier
    ///
    /// # Safety
    /// The caller must ensure that the provided certificates have been properly validated.
    /// Using unvalidated certificates will compromise the type's safety guarantees.
    pub fn from_validated_slow(
        finalize_cert: FinalizeCert,
        notarize_cert: NotarCert,
        bank: &Bank,
    ) -> Self {
        debug_assert_eq!(notarize_cert.block.slot, finalize_cert.slot);

        let finalize_signers = Self::extract_signers(
            bank,
            CertificateType::Finalize(finalize_cert.slot),
            &finalize_cert.signature.bitmap,
        )
        .expect("Certificate should have been validated");
        let mut signers = Self::extract_signers(
            bank,
            CertificateType::Notarize(notarize_cert.block),
            &notarize_cert.signature.bitmap,
        )
        .expect("Certificate should have been validated");
        signers.extend(finalize_signers);
        Self {
            kind: ValidatedBlockFinalizationCertKind::Finalize {
                finalize_cert,
                notarize_cert,
            },
            signers,
        }
    }

    /// Creates a validated fast finalization certificate from an already-validated certificate.
    ///
    /// This is intended for use by the consensus pool, where certificates have already been
    /// validated by the bls sigverifier
    ///
    /// # Safety
    /// The caller must ensure that the provided certificate has been properly validated.
    /// Using an unvalidated certificate will compromise the type's safety guarantees.
    pub fn from_validated_fast(cert: FastFinalizeCert, bank: &Bank) -> Self {
        let cert_type = CertificateType::FinalizeFast(cert.block);
        let signers = Self::extract_signers(bank, cert_type, &cert.signature.bitmap)
            .expect("Certificate should have been validated");
        Self {
            kind: ValidatedBlockFinalizationCertKind::FastFinalize(cert),
            signers,
        }
    }

    /// Returns the slot that is finalized.
    pub fn slot(&self) -> FinalizedSlot {
        match &self.kind {
            ValidatedBlockFinalizationCertKind::Finalize {
                finalize_cert,
                notarize_cert,
            } => {
                debug_assert_eq!(finalize_cert.slot, notarize_cert.block.slot);
                FinalizedSlot::Slow(finalize_cert.slot)
            }
            ValidatedBlockFinalizationCertKind::FastFinalize(certificate) => {
                FinalizedSlot::Fast(certificate.block.slot)
            }
        }
    }

    /// Returns the block that is finalized (slot, block_id).
    pub fn block(&self) -> Block {
        match &self.kind {
            ValidatedBlockFinalizationCertKind::Finalize { notarize_cert, .. } => {
                notarize_cert.block
            }
            ValidatedBlockFinalizationCertKind::FastFinalize(cert) => cert.block,
        }
    }

    /// Returns true if this is a fast finalization.
    pub fn is_fast(&self) -> bool {
        matches!(
            self.kind,
            ValidatedBlockFinalizationCertKind::FastFinalize(_)
        )
    }

    /// Returns cloned copies of the certificates that prove this finalization.
    ///
    /// For slow finalization this returns the finalize certificate followed by the notarize
    /// certificate. For fast finalization this returns the single fast-finalize certificate.
    pub fn clone_certificates(&self) -> Vec<Certificate> {
        match &self.kind {
            ValidatedBlockFinalizationCertKind::Finalize {
                finalize_cert,
                notarize_cert,
            } => {
                let notar = Certificate {
                    cert_type: CertificateType::Notarize(notarize_cert.block),
                    signature: notarize_cert.signature.signature,
                    bitmap: notarize_cert.signature.bitmap.clone(),
                };
                let finalize = Certificate {
                    cert_type: CertificateType::Finalize(finalize_cert.slot),
                    signature: finalize_cert.signature.signature,
                    bitmap: finalize_cert.signature.bitmap.clone(),
                };
                vec![finalize, notar]
            }
            ValidatedBlockFinalizationCertKind::FastFinalize(cert) => {
                let cert = Certificate {
                    cert_type: CertificateType::FinalizeFast(cert.block),
                    signature: cert.signature.signature,
                    bitmap: cert.signature.bitmap.clone(),
                };
                vec![cert]
            }
        }
    }

    /// Returns the data needed to calculating and paying vote rewards.
    pub fn vote_rewards_input(&self) -> (&HashSet<Pubkey>, Slot) {
        (&self.signers, self.slot().slot())
    }

    /// Consumes self and returns the contained certificates and the signers.
    ///
    /// For slow finalization, returns (signers, finalize_cert, Some(notarize_cert)).
    /// For fast finalization, returns (signers, fast_finalize_cert, None).
    pub(crate) fn into_parts(self) -> (HashSet<Pubkey>, Certificate, Option<Certificate>) {
        let signers = self.signers;
        let (final_cert, notar_cert) = match self.kind {
            ValidatedBlockFinalizationCertKind::Finalize {
                finalize_cert,
                notarize_cert,
            } => {
                let notar = Certificate {
                    cert_type: CertificateType::Notarize(notarize_cert.block),
                    signature: notarize_cert.signature.signature,
                    bitmap: notarize_cert.signature.bitmap,
                };
                let finalize = Certificate {
                    cert_type: CertificateType::Finalize(finalize_cert.slot),
                    signature: finalize_cert.signature.signature,
                    bitmap: finalize_cert.signature.bitmap,
                };
                (finalize, Some(notar))
            }
            ValidatedBlockFinalizationCertKind::FastFinalize(cert) => {
                let cert = Certificate {
                    cert_type: CertificateType::FinalizeFast(cert.block),
                    signature: cert.signature.signature,
                    bitmap: cert.signature.bitmap,
                };
                (cert, None)
            }
        };
        (signers, final_cert, notar_cert)
    }

    /// Converts this validated certificate into a [`BlockFinalizationCert`] for inclusion in a block footer.
    pub fn to_block_final_cert(&self) -> BlockFinalizationCert {
        match &self.kind {
            ValidatedBlockFinalizationCertKind::Finalize {
                finalize_cert,
                notarize_cert,
            } => {
                let slot = finalize_cert.slot;
                let block_id = notarize_cert.block.block_id;
                BlockFinalizationCert {
                    slot,
                    block_id,
                    final_aggregate: VotesAggregate::from_cert_signature(
                        finalize_cert.signature.clone(),
                    ),
                    notar_aggregate: Some(VotesAggregate::from_cert_signature(
                        notarize_cert.signature.clone(),
                    )),
                }
            }
            ValidatedBlockFinalizationCertKind::FastFinalize(cert) => {
                let block = cert.block;
                BlockFinalizationCert {
                    slot: block.slot,
                    block_id: block.block_id,
                    final_aggregate: VotesAggregate::from_cert_signature(cert.signature.clone()),
                    notar_aggregate: None,
                }
            }
        }
    }

    fn extract_signers(
        bank: &Bank,
        cert_type: CertificateType,
        bitmap: &[u8],
    ) -> Result<HashSet<Pubkey>, BlockFinalizationCertError> {
        let Some(epoch_stakes) = bank.epoch_stakes_from_slot(cert_type.slot()) else {
            return Err(BlockFinalizationCertError::MissingEpochStakes(cert_type));
        };
        let rank_map = epoch_stakes.bls_pubkey_to_rank_map();
        let decoded = decode(bitmap, rank_map.len());
        let ranks = match decoded {
            Ok(Decoded::Base2(ranks)) => ranks,
            Ok(Decoded::Base3(_, _)) => {
                return Err(BlockFinalizationCertError::Base3Finalization(cert_type));
            }
            Err(err) => return Err(BlockFinalizationCertError::DecodeError(cert_type, err)),
        };

        Ok(ranks
            .iter_ones()
            .filter_map(|rank| {
                rank_map
                    .get_pubkey_stake_entry(rank)
                    .map(|entry| entry.vote_account_pubkey)
            })
            .collect())
    }
}
