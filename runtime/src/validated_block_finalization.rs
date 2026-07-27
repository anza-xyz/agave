//! Validated block finalization certificates.
//!
//! This module provides [`ValidatedBlockFinalizationCert`], a type that represents
//! a validated proof that a block has been finalized. The type can only be constructed
//! through validation, ensuring type-safe guarantees about certificate validity.

use {
    crate::bank::Bank,
    agave_votor_messages::{
        certificate::{
            CertSignature, Certificate, CertificateType, FastFinalizeCert, FinalizeCert, NotarCert,
        },
        consensus_message::Block,
        finalized_slot::FinalizedSlot,
        unverified_vote_message::UnverifiedCertificate,
    },
    solana_bls_signatures::BlsError,
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
    /// BLS signature conversion failed
    #[error("BLS signature conversion for {0:?} failed: {1}")]
    BlsError(CertificateType, BlsError),

    /// Certificate signature verification failed
    #[error("Certificate verification for {0:?} failed")]
    CertificateVerificationFailed(CertificateType),

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
        block_final_cert: BlockFinalizationCert,
        bank: &Bank,
        shred_version: u16,
    ) -> Result<Self, BlockFinalizationCertError> {
        let block = Block {
            slot: block_final_cert.slot,
            block_id: block_final_cert.block_id,
        };

        if let Some(notar_aggregate) = block_final_cert.notar_aggregate {
            // Slow finalization
            let notarize_cert_type = CertificateType::Notarize(block);
            let finalize_cert_type = CertificateType::Finalize(block.slot);

            let unverified_notar_cert = UnverifiedCertificate {
                cert_type: notarize_cert_type,
                signature: notar_aggregate
                    .uncompress_signature()
                    .map_err(|e| BlockFinalizationCertError::BlsError(notarize_cert_type, e))?,
                bitmap: notar_aggregate.into_bitmap(),
                shred_version,
            };
            let unverified_finalize_cert = UnverifiedCertificate {
                cert_type: finalize_cert_type,
                signature: block_final_cert
                    .final_aggregate
                    .uncompress_signature()
                    .map_err(|e| BlockFinalizationCertError::BlsError(finalize_cert_type, e))?,
                bitmap: block_final_cert.final_aggregate.into_bitmap(),
                shred_version,
            };

            // Verify both certificates
            let notar_signature = Self::verify_certificate(bank, unverified_notar_cert)?;
            let finalize_signature = Self::verify_certificate(bank, unverified_finalize_cert)?;

            let finalize_signers =
                Self::extract_signers(bank, finalize_cert_type, &finalize_signature.bitmap)?;
            let mut signers =
                Self::extract_signers(bank, notarize_cert_type, &notar_signature.bitmap)?;
            signers.extend(finalize_signers);

            let notarize_cert = NotarCert {
                block,
                signature: notar_signature,
            };
            let finalize_cert = FinalizeCert {
                slot: block.slot,
                signature: finalize_signature,
            };

            Ok(Self {
                kind: ValidatedBlockFinalizationCertKind::Finalize {
                    finalize_cert,
                    notarize_cert,
                },
                signers,
            })
        } else {
            // Fast finalization
            let fast_finalize_cert_type = CertificateType::FinalizeFast(block);

            let fast_finalize_cert = UnverifiedCertificate {
                cert_type: fast_finalize_cert_type,
                signature: block_final_cert
                    .final_aggregate
                    .uncompress_signature()
                    .map_err(|e| {
                        BlockFinalizationCertError::BlsError(fast_finalize_cert_type, e)
                    })?,
                bitmap: block_final_cert.final_aggregate.into_bitmap(),
                shred_version,
            };

            let signature = Self::verify_certificate(bank, fast_finalize_cert)?;
            let signers = Self::extract_signers(bank, fast_finalize_cert_type, &signature.bitmap)?;
            let fast_finalize_cert = FastFinalizeCert { block, signature };
            Ok(Self {
                kind: ValidatedBlockFinalizationCertKind::FastFinalize(fast_finalize_cert),
                signers,
            })
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

    /// Verifies the certificate and returns `CertSignature` on success.
    fn verify_certificate(
        bank: &Bank,
        cert: UnverifiedCertificate,
    ) -> Result<CertSignature, BlockFinalizationCertError> {
        let cert_type = cert.cert_type;
        let cert = bank
            .verify_certificate(cert)
            .map_err(|_| BlockFinalizationCertError::CertificateVerificationFailed(cert_type))?;
        debug_assert_eq!(cert.cert_type, cert_type);
        Ok(CertSignature {
            signature: cert.signature,
            bitmap: cert.bitmap,
        })
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

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::genesis_utils::{
            ValidatorVoteKeypairs, create_genesis_config_with_alpenglow_vote_accounts,
        },
        agave_votor_messages::{vote::Vote, wire::get_vote_payload_to_sign},
        bitvec::prelude::*,
        rand::Rng,
        solana_bls_signatures::SignatureProjective,
        solana_entry::block_component::VotesAggregate,
        solana_hash::Hash,
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

    /// Build a cert signature by manually aggregating BLS signatures and encoding bitmap.
    /// `signing_ranks` specifies which validator ranks are signing.
    fn build_cert_signature(
        vote: Vote,
        shred_version: u16,
        signing_ranks: &[usize],
        validator_keypairs: &[ValidatorVoteKeypairs],
    ) -> CertSignature {
        let serialized_vote = get_vote_payload_to_sign(vote, shred_version);

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

        CertSignature {
            signature: signature.into(),
            bitmap,
        }
    }

    fn vote_pubkeys_for_ranks(bank: &Bank, ranks: &[usize]) -> HashSet<Pubkey> {
        let rank_map = bank
            .epoch_stakes_from_slot(bank.slot())
            .unwrap()
            .bls_pubkey_to_rank_map();
        ranks
            .iter()
            .map(|rank| {
                rank_map
                    .get_pubkey_stake_entry(*rank)
                    .unwrap()
                    .vote_account_pubkey
            })
            .collect()
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
        let shred_version = rand::rng().random();

        let block = Block {
            slot: bank.slot(),
            block_id: Hash::new_unique(),
        };

        // Test 1: Fast finalize (requires 80% stake = 4400)
        // Top 6 validators = 1000+900+800+700+600+500 = 4500 (>= 80%)
        {
            let cert_type = CertificateType::FinalizeFast(block);
            let vote = Vote::new_notarization_vote(block);
            let signing_ranks: Vec<usize> = (0..6).collect();
            let fast_finalize_cert_sig =
                build_cert_signature(vote, shred_version, &signing_ranks, &validator_keypairs);
            let fast_finalize_cert = Certificate {
                cert_type,
                signature: fast_finalize_cert_sig.signature,
                bitmap: fast_finalize_cert_sig.bitmap.clone(),
            };

            let block_final_cert = BlockFinalizationCert {
                slot: block.slot,
                block_id: block.block_id,
                final_aggregate: VotesAggregate::from_cert_signature(fast_finalize_cert_sig),
                notar_aggregate: None,
            };

            let validated = ValidatedBlockFinalizationCert::try_from_footer(
                block_final_cert,
                &bank,
                shred_version,
            )
            .expect("Valid fast finalize certificate should pass verification");
            assert_eq!(validated.clone_certificates(), vec![fast_finalize_cert]);
        }

        // Test 2: Slow finalize (requires 60% stake = 3300 for both certs)
        // Top 4 validators = 1000+900+800+700 = 3400 (>= 60%)
        {
            let notarize_cert_type = CertificateType::Notarize(block);
            let notarize_vote = Vote::new_notarization_vote(block);
            let notarize_signing_ranks: Vec<usize> = (0..4).collect();
            let notarize_cert_sig = build_cert_signature(
                notarize_vote,
                shred_version,
                &notarize_signing_ranks,
                &validator_keypairs,
            );
            let notarize_cert = Certificate {
                cert_type: notarize_cert_type,
                signature: notarize_cert_sig.signature,
                bitmap: notarize_cert_sig.bitmap.clone(),
            };

            let finalize_cert_type = CertificateType::Finalize(block.slot);
            let finalize_vote = Vote::new_finalization_vote(block.slot);
            let finalize_signing_ranks: Vec<usize> = (0..4).collect();
            let finalize_cert_sig = build_cert_signature(
                finalize_vote,
                shred_version,
                &finalize_signing_ranks,
                &validator_keypairs,
            );
            let finalize_cert = Certificate {
                cert_type: finalize_cert_type,
                signature: finalize_cert_sig.signature,
                bitmap: finalize_cert_sig.bitmap.clone(),
            };

            let block_final_cert = BlockFinalizationCert {
                slot: block.slot,
                block_id: block.block_id,
                final_aggregate: VotesAggregate::from_cert_signature(finalize_cert_sig),
                notar_aggregate: Some(VotesAggregate::from_cert_signature(notarize_cert_sig)),
            };

            let validated = ValidatedBlockFinalizationCert::try_from_footer(
                block_final_cert,
                &bank,
                shred_version,
            )
            .expect("Valid slow finalize certificate should pass verification");
            assert_eq!(
                validated.clone_certificates(),
                vec![finalize_cert, notarize_cert]
            );
        }
    }

    #[test]
    fn test_slow_finalization_signers_include_notarize_and_finalize_signers() {
        let stakes = vec![1000, 900, 800, 700, 100];
        let (bank, validator_keypairs) = create_bank_with_bls_validators(stakes.len(), stakes);
        let shred_version = rand::rng().random();

        let block = Block {
            slot: bank.slot(),
            block_id: Hash::new_unique(),
        };

        let notarize_vote = Vote::new_notarization_vote(block);
        let notarize_signing_ranks = vec![0, 1, 3];
        let notarize_cert = NotarCert {
            block,
            signature: build_cert_signature(
                notarize_vote,
                shred_version,
                &notarize_signing_ranks,
                &validator_keypairs,
            ),
        };

        let finalize_vote = Vote::new_finalization_vote(block.slot);
        let finalize_signing_ranks = vec![0, 1, 2];
        let finalize_cert = FinalizeCert {
            slot: block.slot,
            signature: build_cert_signature(
                finalize_vote,
                shred_version,
                &finalize_signing_ranks,
                &validator_keypairs,
            ),
        };

        let mut expected_signers = vote_pubkeys_for_ranks(&bank, &notarize_signing_ranks);
        expected_signers.extend(vote_pubkeys_for_ranks(&bank, &finalize_signing_ranks));

        let block_final_cert = BlockFinalizationCert {
            slot: block.slot,
            block_id: block.block_id,
            final_aggregate: VotesAggregate::from_cert_signature(finalize_cert.signature.clone()),
            notar_aggregate: Some(VotesAggregate::from_cert_signature(
                notarize_cert.signature.clone(),
            )),
        };
        let validated =
            ValidatedBlockFinalizationCert::try_from_footer(block_final_cert, &bank, shred_version)
                .expect("Valid slow finalize certificate should pass verification");
        let (signers, _, _) = validated.into_parts();
        assert_eq!(signers, expected_signers);

        let validated = ValidatedBlockFinalizationCert::from_validated_slow(
            finalize_cert,
            notarize_cert,
            &bank,
        );
        let (signers, _, _) = validated.into_parts();
        assert_eq!(signers, expected_signers);
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
        let shred_version = rand::rng().random();

        let block = Block {
            slot: bank.slot(),
            block_id: Hash::new_unique(),
        };

        // Fast finalize with insufficient stake (requires 80% = 4400)
        // Top 5 validators = 1000+900+800+700+600 = 4000 (< 80%)
        {
            let vote = Vote::new_notarization_vote(block);
            let signing_ranks: Vec<usize> = (0..5).collect();
            let fast_finalize_cert_sig =
                build_cert_signature(vote, shred_version, &signing_ranks, &validator_keypairs);

            let block_final_cert = BlockFinalizationCert {
                slot: block.slot,
                block_id: block.block_id,
                final_aggregate: VotesAggregate::from_cert_signature(fast_finalize_cert_sig),
                notar_aggregate: None,
            };

            let err = ValidatedBlockFinalizationCert::try_from_footer(
                block_final_cert,
                &bank,
                shred_version,
            )
            .unwrap_err();
            assert!(
                matches!(
                    err,
                    BlockFinalizationCertError::CertificateVerificationFailed { .. }
                ),
                "Fast finalize with insufficient stake should fail verification.  {err:?}"
            );
        }

        // Slow finalize with insufficient notarize stake (requires 60% = 3300)
        // Top 3 validators = 1000+900+800 = 2700 (< 60%)
        {
            let notarize_vote = Vote::new_notarization_vote(block);
            let notarize_signing_ranks: Vec<usize> = (0..3).collect();
            let notarize_cert_sig = build_cert_signature(
                notarize_vote,
                shred_version,
                &notarize_signing_ranks,
                &validator_keypairs,
            );

            // Finalize cert has enough stake
            let finalize_vote = Vote::new_finalization_vote(block.slot);
            let finalize_signing_ranks: Vec<usize> = (0..4).collect();
            let finalize_cert_sig = build_cert_signature(
                finalize_vote,
                shred_version,
                &finalize_signing_ranks,
                &validator_keypairs,
            );

            let block_final_cert = BlockFinalizationCert {
                slot: block.slot,
                block_id: block.block_id,
                final_aggregate: VotesAggregate::from_cert_signature(finalize_cert_sig),
                notar_aggregate: Some(VotesAggregate::from_cert_signature(notarize_cert_sig)),
            };

            let err = ValidatedBlockFinalizationCert::try_from_footer(
                block_final_cert,
                &bank,
                shred_version,
            )
            .unwrap_err();
            assert!(
                matches!(
                    err,
                    BlockFinalizationCertError::CertificateVerificationFailed { .. }
                ),
                "Slow finalize with insufficient notarize stake should fail verification"
            );
        }

        // Slow finalize with insufficient finalize stake (requires 60% = 3300)
        // Notarize has enough stake, but finalize doesn't
        {
            // Notarize cert has enough stake
            let notarize_vote = Vote::new_notarization_vote(block);
            let notarize_signing_ranks: Vec<usize> = (0..4).collect();
            let notarize_cert_sig = build_cert_signature(
                notarize_vote,
                shred_version,
                &notarize_signing_ranks,
                &validator_keypairs,
            );

            // Finalize cert has insufficient stake
            // Top 3 validators = 1000+900+800 = 2700 (< 60%)
            let finalize_vote = Vote::new_finalization_vote(block.slot);
            let finalize_signing_ranks: Vec<usize> = (0..3).collect();
            let finalize_cert_sig = build_cert_signature(
                finalize_vote,
                shred_version,
                &finalize_signing_ranks,
                &validator_keypairs,
            );

            let block_final_cert = BlockFinalizationCert {
                slot: block.slot,
                block_id: block.block_id,
                final_aggregate: VotesAggregate::from_cert_signature(finalize_cert_sig),
                notar_aggregate: Some(VotesAggregate::from_cert_signature(notarize_cert_sig)),
            };

            let err = ValidatedBlockFinalizationCert::try_from_footer(
                block_final_cert,
                &bank,
                shred_version,
            )
            .unwrap_err();
            assert!(
                matches!(
                    err,
                    BlockFinalizationCertError::CertificateVerificationFailed { .. }
                ),
                "Slow finalize with insufficient finalize stake should fail verification"
            );
        }
    }
}
