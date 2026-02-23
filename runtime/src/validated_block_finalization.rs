//! Validated block finalization certificates.
//!
//! This module provides [`ValidatedBlockFinalizationCert`], a type that represents
//! a validated proof that a block has been finalized. The type can only be constructed
//! through validation, ensuring type-safe guarantees about certificate validity.

use {
    crate::bank::Bank,
    agave_votor_messages::{
        consensus_message::{Certificate, CertificateType},
        fraction::Fraction,
    },
    log::warn,
    solana_bls_signatures::BlsError,
    solana_clock::Slot,
    solana_entry::block_component::{FinalCertificate, VotesAggregate},
    solana_hash::Hash,
    std::num::NonZeroU64,
    thiserror::Error,
};

/// Error type for block finalization certificate validation.
#[derive(Debug, Error)]
pub enum BlockFinalizationCertError {
    /// BLS signature conversion failed
    #[error("BLS signature conversion for {0:?} failed: {1}")]
    BlsError(CertificateType, BlsError),

    /// Certificate signature verification failed
    #[error("Certificate signature for {0:?} verification failed")]
    SignatureVerificationFailed(CertificateType),

    /// Insufficient stake for finalization
    #[error("Insufficient stake for {cert:?}: got {got}, required {required}")]
    InsufficientStake {
        cert: CertificateType,
        got: Fraction,
        required: Fraction,
    },
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
        finalize_cert: Certificate,
        /// The notarize certificate
        notarize_cert: Certificate,
    },
    /// Fast finalization: a single FastFinalize certificate
    FastFinalize(Certificate),
}

/// A validated proof that a block has been finalized.
///
/// This type can only be constructed through validation methods, providing
/// compile-time guarantees that the contained certificates are valid.
#[derive(Clone, Debug)]
pub struct ValidatedBlockFinalizationCert(ValidatedBlockFinalizationCertKind);

impl ValidatedBlockFinalizationCert {
    /// Creates a validated block finalization certificate from a footer's final certificate.
    ///
    /// This method performs both BLS signature conversion and stake threshold verification
    /// against the provided bank's epoch stakes.
    ///
    /// # Errors
    /// Returns an error if:
    /// - BLS signature conversion fails
    /// - Certificate signature verification fails
    /// - The certificates don't meet the required stake thresholds
    pub fn try_from_footer(
        final_cert: FinalCertificate,
        bank: &Bank,
    ) -> Result<Self, BlockFinalizationCertError> {
        let (slot, block_id) = (final_cert.slot, final_cert.block_id);

        // Convert the serialized form to certificates
        let validated =
            if let Some(notar_aggregate) = final_cert.notar_aggregate {
                // Slow finalization
                let notarize_cert_type = CertificateType::Notarize(slot, block_id);
                let finalize_cert_type = CertificateType::Finalize(slot);

                let notarize_cert = Certificate {
                    cert_type: notarize_cert_type,
                    signature: notar_aggregate
                        .uncompress_signature()
                        .map_err(|e| BlockFinalizationCertError::BlsError(notarize_cert_type, e))?,
                    bitmap: notar_aggregate.into_bitmap(),
                };
                let finalize_cert = Certificate {
                    cert_type: finalize_cert_type,
                    signature: final_cert
                        .final_aggregate
                        .uncompress_signature()
                        .map_err(|e| BlockFinalizationCertError::BlsError(finalize_cert_type, e))?,
                    bitmap: final_cert.final_aggregate.into_bitmap(),
                };

                // Verify both certificates
                let (notarize_stake, total_stake) = Self::verify_certificate(bank, &notarize_cert)?;
                let (finalize_stake, _) = Self::verify_certificate(bank, &finalize_cert)?;

                let notarize_percent =
                    Fraction::new(notarize_stake, NonZeroU64::new(total_stake).unwrap());
                let finalize_percent =
                    Fraction::new(finalize_stake, NonZeroU64::new(total_stake).unwrap());
                let notarize_threshold = notarize_cert.cert_type.limits_and_vote_types().0;
                let finalize_threshold = finalize_cert.cert_type.limits_and_vote_types().0;

                if notarize_percent < notarize_threshold {
                    warn!(
                        "Received a slow finalization in the footer for slot {slot} block_id \
                         {block_id:?} in bank slot {} with notarize {notarize_percent} stake \
                         expecting at least {notarize_threshold}",
                        bank.slot()
                    );
                    return Err(BlockFinalizationCertError::InsufficientStake {
                        cert: notarize_cert_type,
                        got: notarize_percent,
                        required: notarize_threshold,
                    });
                }

                if finalize_percent < finalize_threshold {
                    warn!(
                        "Received a slow finalization in the footer for slot {slot} block_id \
                         {block_id:?} in bank slot {} with finalize {finalize_percent} stake \
                         expecting at least {finalize_threshold}",
                        bank.slot()
                    );
                    return Err(BlockFinalizationCertError::InsufficientStake {
                        cert: finalize_cert_type,
                        got: finalize_percent,
                        required: finalize_threshold,
                    });
                }

                Self(ValidatedBlockFinalizationCertKind::Finalize {
                    finalize_cert,
                    notarize_cert,
                })
            } else {
                // Fast finalization
                let fast_finalize_cert_type = CertificateType::FinalizeFast(slot, block_id);

                let fast_finalize_cert =
                    Certificate {
                        cert_type: fast_finalize_cert_type,
                        signature: final_cert.final_aggregate.uncompress_signature().map_err(
                            |e| BlockFinalizationCertError::BlsError(fast_finalize_cert_type, e),
                        )?,
                        bitmap: final_cert.final_aggregate.into_bitmap(),
                    };

                let (finalize_stake, total_stake) =
                    Self::verify_certificate(bank, &fast_finalize_cert)?;

                let finalize_percent =
                    Fraction::new(finalize_stake, NonZeroU64::new(total_stake).unwrap());
                let finalize_threshold = fast_finalize_cert.cert_type.limits_and_vote_types().0;

                if finalize_percent < finalize_threshold {
                    warn!(
                        "Received a fast finalization in the footer for slot {slot} block_id \
                         {block_id:?} in bank slot {} with {finalize_percent} stake expecting at \
                         least {finalize_threshold}",
                        bank.slot()
                    );
                    return Err(BlockFinalizationCertError::InsufficientStake {
                        cert: fast_finalize_cert_type,
                        got: finalize_percent,
                        required: finalize_threshold,
                    });
                }

                Self(ValidatedBlockFinalizationCertKind::FastFinalize(
                    fast_finalize_cert,
                ))
            };

        Ok(validated)
    }

    /// Creates a validated block finalization certificate from already-validated certificates.
    ///
    /// This is intended for use by the consensus pool, where certificates have already been
    /// validated by the bls sigverifier
    ///
    /// # Safety
    /// The caller must ensure that the provided certificates have been properly validated.
    /// Using unvalidated certificates will compromise the type's safety guarantees.
    pub fn from_validated_slow(finalize_cert: Certificate, notarize_cert: Certificate) -> Self {
        debug_assert!(finalize_cert.cert_type.is_slow_finalization());
        debug_assert!(notarize_cert.cert_type.is_notarize());
        debug_assert_eq!(
            notarize_cert.cert_type.slot(),
            finalize_cert.cert_type.slot()
        );
        Self(ValidatedBlockFinalizationCertKind::Finalize {
            finalize_cert,
            notarize_cert,
        })
    }

    /// Creates a validated fast finalization certificate from an already-validated certificate.
    ///
    /// This is intended for use by the consensus pool, where certificates have already been
    /// validated by the bls sigverifier
    ///
    /// # Safety
    /// The caller must ensure that the provided certificate has been properly validated.
    /// Using an unvalidated certificate will compromise the type's safety guarantees.
    pub fn from_validated_fast(cert: Certificate) -> Self {
        debug_assert!(cert.cert_type.is_fast_finalization());
        Self(ValidatedBlockFinalizationCertKind::FastFinalize(cert))
    }

    /// Returns the slot that is finalized.
    pub fn slot(&self) -> Slot {
        match &self.0 {
            ValidatedBlockFinalizationCertKind::Finalize {
                finalize_cert,
                notarize_cert,
            } => {
                debug_assert_eq!(
                    finalize_cert.cert_type.slot(),
                    notarize_cert.cert_type.slot()
                );
                finalize_cert.cert_type.slot()
            }
            ValidatedBlockFinalizationCertKind::FastFinalize(certificate) => {
                certificate.cert_type.slot()
            }
        }
    }

    /// Returns the block that is finalized (slot, block_id).
    pub fn block(&self) -> (Slot, Hash) {
        match &self.0 {
            ValidatedBlockFinalizationCertKind::Finalize { notarize_cert, .. } => notarize_cert
                .cert_type
                .to_block()
                .expect("notarize certificate has block"),
            ValidatedBlockFinalizationCertKind::FastFinalize(cert) => cert
                .cert_type
                .to_block()
                .expect("fast finalize certificate has block"),
        }
    }

    /// Returns true if this is a fast finalization.
    pub fn is_fast(&self) -> bool {
        matches!(self.0, ValidatedBlockFinalizationCertKind::FastFinalize(_))
    }

    /// Consumes self and returns the contained certificates.
    ///
    /// For slow finalization, returns (finalize_cert, Some(notarize_cert)).
    /// For fast finalization, returns (fast_finalize_cert, None).
    pub fn into_certificates(self) -> (Certificate, Option<Certificate>) {
        match self.0 {
            ValidatedBlockFinalizationCertKind::Finalize {
                finalize_cert,
                notarize_cert,
            } => (finalize_cert, Some(notarize_cert)),
            ValidatedBlockFinalizationCertKind::FastFinalize(cert) => (cert, None),
        }
    }

    /// Converts this validated certificate into a [`FinalCertificate`] for inclusion in a block footer.
    pub fn to_final_certificate(&self) -> FinalCertificate {
        match &self.0 {
            ValidatedBlockFinalizationCertKind::Finalize {
                finalize_cert,
                notarize_cert,
            } => {
                let slot = finalize_cert.cert_type.slot();
                let block_id = notarize_cert
                    .cert_type
                    .to_block()
                    .expect("notarize certificates correspond to blocks")
                    .1;
                FinalCertificate {
                    slot,
                    block_id,
                    final_aggregate: VotesAggregate::from_certificate(finalize_cert),
                    notar_aggregate: Some(VotesAggregate::from_certificate(notarize_cert)),
                }
            }
            ValidatedBlockFinalizationCertKind::FastFinalize(cert) => {
                let (slot, block_id) = cert
                    .cert_type
                    .to_block()
                    .expect("fast finalizations correspond to blocks");
                FinalCertificate {
                    slot,
                    block_id,
                    final_aggregate: VotesAggregate::from_certificate(cert),
                    notar_aggregate: None,
                }
            }
        }
    }

    /// Verifies the certificate, returning (stake present in certificate, total stake in validator set) on success.
    fn verify_certificate(
        bank: &Bank,
        cert: &Certificate,
    ) -> Result<(u64, u64), BlockFinalizationCertError> {
        bank.verify_certificate(cert)
            .map_err(|_| BlockFinalizationCertError::SignatureVerificationFailed(cert.cert_type))
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::genesis_utils::{
            ValidatorVoteKeypairs, create_genesis_config_with_alpenglow_vote_accounts,
        },
        agave_votor_messages::vote::Vote,
        bitvec::prelude::*,
        solana_bls_signatures::SignatureProjective,
        solana_entry::block_component::VotesAggregate,
        solana_signer_store::encode_base2,
        std::sync::Arc,
    };

    /// Creates a bank with BLS-enabled validators for testing certificate verification.
    /// Returns (bank, validator_keypairs) where bank has validators with BLS keys.
    fn create_bank_with_bls_validators(
        num_validators: usize,
        stakes: Vec<u64>,
    ) -> (Arc<Bank>, Vec<ValidatorVoteKeypairs>) {
        assert_eq!(num_validators, stakes.len());
        let validator_keypairs: Vec<ValidatorVoteKeypairs> = (0..num_validators)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect();

        let genesis_config_info = create_genesis_config_with_alpenglow_vote_accounts(
            10_000_000,
            &validator_keypairs,
            stakes,
        );

        let bank = Arc::new(Bank::new_for_tests(&genesis_config_info.genesis_config));
        (bank, validator_keypairs)
    }

    /// Build a certificate by manually aggregating BLS signatures and encoding bitmap.
    /// `signing_ranks` specifies which validator ranks are signing.
    fn build_certificate_manual(
        cert_type: CertificateType,
        vote: Vote,
        signing_ranks: &[usize],
        validator_keypairs: &[ValidatorVoteKeypairs],
    ) -> Certificate {
        let serialized_vote = bincode::serialize(&vote).unwrap();

        // Aggregate signatures
        let mut signature = SignatureProjective::identity();
        for &rank in signing_ranks {
            let sig = validator_keypairs[rank].bls_keypair.sign(&serialized_vote);
            signature.aggregate_with(std::iter::once(&sig)).unwrap();
        }

        // Build bitmap
        let max_rank = signing_ranks.iter().copied().max().unwrap_or(0);
        let mut bitvec = BitVec::<u8, Lsb0>::repeat(false, max_rank.saturating_add(1));
        for &rank in signing_ranks {
            bitvec.set(rank, true);
        }
        let bitmap = encode_base2(&bitvec).expect("Failed to encode bitmap");

        Certificate {
            cert_type,
            signature: signature.into(),
            bitmap,
        }
    }

    #[test]
    fn test_verify_final_cert_valid() {
        // Create 10 validators with descending stakes (1000, 900, 800, ...)
        // Total stake = 5500
        let num_validators = 10;
        let stakes: Vec<u64> = (0..num_validators)
            .map(|i| (1000u64).saturating_sub((i as u64).saturating_mul(100)))
            .collect();
        let (bank, validator_keypairs) = create_bank_with_bls_validators(num_validators, stakes);

        let slot = bank.slot();
        let block_id = Hash::new_unique();

        // Test 1: Fast finalize (requires 80% stake = 4400)
        // Top 6 validators = 1000+900+800+700+600+500 = 4500 (>= 80%)
        {
            let cert_type = CertificateType::FinalizeFast(slot, block_id);
            let vote = Vote::new_notarization_vote(slot, block_id);
            let signing_ranks: Vec<usize> = (0..6).collect();
            let fast_finalize_cert =
                build_certificate_manual(cert_type, vote, &signing_ranks, &validator_keypairs);

            let final_cert = FinalCertificate {
                slot,
                block_id,
                final_aggregate: VotesAggregate::from_certificate(&fast_finalize_cert),
                notar_aggregate: None,
            };

            let result = ValidatedBlockFinalizationCert::try_from_footer(final_cert, &bank);
            assert!(
                result.is_ok(),
                "Valid fast finalize certificate should pass verification: {result:?}"
            );
        }

        // Test 2: Slow finalize (requires 60% stake = 3300 for both certs)
        // Top 4 validators = 1000+900+800+700 = 3400 (>= 60%)
        {
            let notarize_cert_type = CertificateType::Notarize(slot, block_id);
            let notarize_vote = Vote::new_notarization_vote(slot, block_id);
            let notarize_signing_ranks: Vec<usize> = (0..4).collect();
            let notarize_cert = build_certificate_manual(
                notarize_cert_type,
                notarize_vote,
                &notarize_signing_ranks,
                &validator_keypairs,
            );

            let finalize_cert_type = CertificateType::Finalize(slot);
            let finalize_vote = Vote::new_finalization_vote(slot);
            let finalize_signing_ranks: Vec<usize> = (0..4).collect();
            let finalize_cert = build_certificate_manual(
                finalize_cert_type,
                finalize_vote,
                &finalize_signing_ranks,
                &validator_keypairs,
            );

            let final_cert = FinalCertificate {
                slot,
                block_id,
                final_aggregate: VotesAggregate::from_certificate(&finalize_cert),
                notar_aggregate: Some(VotesAggregate::from_certificate(&notarize_cert)),
            };

            let result = ValidatedBlockFinalizationCert::try_from_footer(final_cert, &bank);
            assert!(
                result.is_ok(),
                "Valid slow finalize certificate should pass verification: {result:?}"
            );
        }
    }

    #[test]
    fn test_verify_final_cert_insufficient_stake() {
        // Create 10 validators with descending stakes (1000, 900, 800, ...)
        // Total stake = 5500
        let num_validators = 10;
        let stakes: Vec<u64> = (0..num_validators)
            .map(|i| (1000u64).saturating_sub((i as u64).saturating_mul(100)))
            .collect();
        let (bank, validator_keypairs) = create_bank_with_bls_validators(num_validators, stakes);

        let slot = bank.slot();
        let block_id = Hash::new_unique();

        // Fast finalize with insufficient stake (requires 80% = 4400)
        // Top 5 validators = 1000+900+800+700+600 = 4000 (< 80%)
        {
            let cert_type = CertificateType::FinalizeFast(slot, block_id);
            let vote = Vote::new_notarization_vote(slot, block_id);
            let signing_ranks: Vec<usize> = (0..5).collect();
            let fast_finalize_cert =
                build_certificate_manual(cert_type, vote, &signing_ranks, &validator_keypairs);

            let final_cert = FinalCertificate {
                slot,
                block_id,
                final_aggregate: VotesAggregate::from_certificate(&fast_finalize_cert),
                notar_aggregate: None,
            };

            let result = ValidatedBlockFinalizationCert::try_from_footer(final_cert, &bank);
            assert!(
                matches!(
                    result,
                    Err(BlockFinalizationCertError::InsufficientStake { .. })
                ),
                "Fast finalize with insufficient stake should fail verification"
            );
        }

        // Slow finalize with insufficient notarize stake (requires 60% = 3300)
        // Top 3 validators = 1000+900+800 = 2700 (< 60%)
        {
            let notarize_cert_type = CertificateType::Notarize(slot, block_id);
            let notarize_vote = Vote::new_notarization_vote(slot, block_id);
            let notarize_signing_ranks: Vec<usize> = (0..3).collect();
            let notarize_cert = build_certificate_manual(
                notarize_cert_type,
                notarize_vote,
                &notarize_signing_ranks,
                &validator_keypairs,
            );

            // Finalize cert has enough stake
            let finalize_cert_type = CertificateType::Finalize(slot);
            let finalize_vote = Vote::new_finalization_vote(slot);
            let finalize_signing_ranks: Vec<usize> = (0..4).collect();
            let finalize_cert = build_certificate_manual(
                finalize_cert_type,
                finalize_vote,
                &finalize_signing_ranks,
                &validator_keypairs,
            );

            let final_cert = FinalCertificate {
                slot,
                block_id,
                final_aggregate: VotesAggregate::from_certificate(&finalize_cert),
                notar_aggregate: Some(VotesAggregate::from_certificate(&notarize_cert)),
            };

            let result = ValidatedBlockFinalizationCert::try_from_footer(final_cert, &bank);
            assert!(
                matches!(
                    result,
                    Err(BlockFinalizationCertError::InsufficientStake { .. })
                ),
                "Slow finalize with insufficient notarize stake should fail verification"
            );
        }

        // Slow finalize with insufficient finalize stake (requires 60% = 3300)
        // Notarize has enough stake, but finalize doesn't
        {
            // Notarize cert has enough stake
            let notarize_cert_type = CertificateType::Notarize(slot, block_id);
            let notarize_vote = Vote::new_notarization_vote(slot, block_id);
            let notarize_signing_ranks: Vec<usize> = (0..4).collect();
            let notarize_cert = build_certificate_manual(
                notarize_cert_type,
                notarize_vote,
                &notarize_signing_ranks,
                &validator_keypairs,
            );

            // Finalize cert has insufficient stake
            // Top 3 validators = 1000+900+800 = 2700 (< 60%)
            let finalize_cert_type = CertificateType::Finalize(slot);
            let finalize_vote = Vote::new_finalization_vote(slot);
            let finalize_signing_ranks: Vec<usize> = (0..3).collect();
            let finalize_cert = build_certificate_manual(
                finalize_cert_type,
                finalize_vote,
                &finalize_signing_ranks,
                &validator_keypairs,
            );

            let final_cert = FinalCertificate {
                slot,
                block_id,
                final_aggregate: VotesAggregate::from_certificate(&finalize_cert),
                notar_aggregate: Some(VotesAggregate::from_certificate(&notarize_cert)),
            };

            let result = ValidatedBlockFinalizationCert::try_from_footer(final_cert, &bank);
            assert!(
                matches!(
                    result,
                    Err(BlockFinalizationCertError::InsufficientStake { .. })
                ),
                "Slow finalize with insufficient finalize stake should fail verification"
            );
        }
    }
}
