use {
    crate::{
        bank::Bank,
        block_component_processor::validated_block_certs::{
            validated_block_finalization::ValidatedBlockFinalizationCert,
            validated_reward_certificate::{InvalidSlots, ValidatedRewardCert, extract_slot},
        },
        epoch_stakes::BLSPubkeyToRankMap,
    },
    agave_votor_messages::{
        certificate::{CertSignature, CertificateType, FastFinalizeCert, FinalizeCert, NotarCert},
        consensus_message::{Block, BlockId},
        fraction::Fraction,
        reward_certificate::{NotarRewardCertificate, SkipRewardCertificate},
        vote::Vote,
        wire::get_vote_payload_to_sign,
    },
    itertools::Either,
    rayon::{
        ThreadPool,
        iter::{IntoParallelIterator, IntoParallelRefIterator, ParallelIterator},
    },
    solana_bls_signatures::{
        PubkeyProjective, Signature as BLSSignature, SignatureCompressed, VerifySignature,
        pubkey::{PopVerified, PubkeyAffine},
        signature::AsSignatureAffine,
    },
    solana_clock::Slot,
    solana_entry::block_component::BlockFinalizationCert,
    solana_pubkey::Pubkey,
    solana_signer_store::{DecodeError, Decoded, decode},
    std::collections::HashSet,
    thiserror::Error,
};

pub mod validated_block_finalization;
pub mod validated_reward_certificate;

#[derive(Debug, Error)]
pub enum ValidatedBlockCertsError {
    #[error("decoding failed with {0:?}")]
    DecodeFailed(DecodeError),
    #[error("invalid rank")]
    InvalidRank,
    #[error("saw a base3 cert")]
    Base3Cert,
    #[error("rank map not found")]
    RankMapNotFound,
    #[error("reward slots mismatch")]
    RewardSlotMismatch,
    #[error("stake threshold not met")]
    StakeThresholdNotMet,
    #[error("pubkey aggregation failed")]
    PubkeyAggregationFailed,
    #[error("signature verification failed")]
    SignatureVerificationFailed,
    #[error("invalid reward slots {0:?}")]
    InvalidRewardSlots(InvalidSlots),
    #[error("uncompressing signature failed")]
    UncompressSignatureFailed,
}

pub(super) fn verify_certs(
    thread_pool: &ThreadPool,
    bank: &Bank,
    shred_version: u16,
    block_cert: Option<BlockFinalizationCert>,
    skip_reward_cert: Option<SkipRewardCertificate>,
    notar_reward_cert: Option<NotarRewardCertificate>,
) -> Result<
    (
        Option<ValidatedRewardCert>,
        Option<ValidatedBlockFinalizationCert>,
    ),
    ValidatedBlockCertsError,
> {
    let mut payloads = Vec::with_capacity(4);
    let reward_state = get_reward_payloads(
        bank,
        shred_version,
        &skip_reward_cert,
        &notar_reward_cert,
        &mut payloads,
    )?;
    let block_cert_validators =
        get_final_payloads(bank, shred_version, &block_cert, &mut payloads)?;

    thread_pool.install(|| {
        payloads.into_par_iter().try_for_each(|payload| {
            let pubkeys = payload.pubkeys;
            let agg_pubkey = PubkeyProjective::par_aggregate(pubkeys.par_iter())
                .map_err(|_| ValidatedBlockCertsError::PubkeyAggregationFailed)?;
            agg_pubkey
                .verify_signature(payload.signature, &payload.message)
                .map_err(|_| ValidatedBlockCertsError::SignatureVerificationFailed)
        })
    })?;

    let validated_block_cert = if let Some(block_cert) = block_cert {
        let block = Block {
            slot: block_cert.slot,
            block_id: block_cert.block_id,
        };
        match block_cert.notar_aggregate {
            None => {
                let (signature, bitmap) = block_cert.final_aggregate.into_parts();
                let signature = CertSignature {
                    signature: uncompress_signature(signature)?,
                    bitmap,
                };
                let fast_final = FastFinalizeCert { block, signature };
                Some(ValidatedBlockFinalizationCert::try_from_validated(
                    Either::Left(fast_final),
                    block_cert_validators,
                ))
            }
            Some(notar_aggregate) => {
                let (signature, bitmap) = block_cert.final_aggregate.into_parts();
                let signature = CertSignature {
                    signature: uncompress_signature(signature)?,
                    bitmap,
                };
                let final_cert = FinalizeCert {
                    slot: block_cert.slot,
                    signature,
                };
                let (signature, bitmap) = notar_aggregate.into_parts();
                let signature = CertSignature {
                    signature: uncompress_signature(signature)?,
                    bitmap,
                };
                let notar_cert = NotarCert { block, signature };
                Some(ValidatedBlockFinalizationCert::try_from_validated(
                    Either::Right((notar_cert, final_cert)),
                    block_cert_validators,
                ))
            }
        }
    } else {
        None
    };

    let validated_reward_cert =
        reward_state.map(|(slot, validators)| ValidatedRewardCert::new_validated(validators, slot));
    Ok((validated_reward_cert, validated_block_cert))
}

fn uncompress_signature(
    signature: SignatureCompressed,
) -> Result<BLSSignature, ValidatedBlockCertsError> {
    Ok(BLSSignature::from(signature.try_as_affine().map_err(
        |_| ValidatedBlockCertsError::UncompressSignatureFailed,
    )?))
}

struct Stake(u64);

fn get_pubkeys(
    rank_map: &BLSPubkeyToRankMap,
    bitmap: &[u8],
    validators: &mut HashSet<Pubkey>,
) -> Result<(Vec<PopVerified<PubkeyAffine>>, Stake), ValidatedBlockCertsError> {
    let max_validators = rank_map.len();
    let ranks = decode(bitmap, max_validators).map_err(ValidatedBlockCertsError::DecodeFailed)?;
    let ranks = match ranks {
        Decoded::Base2(ranks) => ranks,
        Decoded::Base3(_, _) => return Err(ValidatedBlockCertsError::Base3Cert),
    };
    let mut pubkeys = Vec::with_capacity(ranks.count_ones());
    let mut stake = 0;
    for rank in ranks.iter_ones() {
        let rank_map_entry = rank_map
            .get_pubkey_stake_entry(rank)
            .ok_or(ValidatedBlockCertsError::InvalidRank)?;
        pubkeys.push(rank_map_entry.bls_pubkey);
        validators.insert(rank_map_entry.vote_account_pubkey);
        stake += rank_map_entry.stake.get();
    }
    Ok((pubkeys, Stake(stake)))
}

fn get_rank_map(bank: &Bank, slot: Slot) -> Result<&BLSPubkeyToRankMap, ValidatedBlockCertsError> {
    match bank.epoch_stakes_from_slot(slot) {
        None => Err(ValidatedBlockCertsError::RankMapNotFound),
        Some(s) => Ok(s.bls_pubkey_to_rank_map()),
    }
}

struct Payload<'a> {
    pubkeys: Vec<PopVerified<PubkeyAffine>>,
    signature: &'a SignatureCompressed,
    message: Vec<u8>,
}

fn get_reward_payloads<'a>(
    bank: &Bank,
    shred_version: u16,
    skip_reward_cert: &'a Option<SkipRewardCertificate>,
    notar_reward_cert: &'a Option<NotarRewardCertificate>,
    payloads: &mut Vec<Payload<'a>>,
) -> Result<Option<(Slot, HashSet<Pubkey>)>, ValidatedBlockCertsError> {
    extract_slot(bank, skip_reward_cert, notar_reward_cert)
        .map_err(ValidatedBlockCertsError::InvalidRewardSlots)?;
    let ret = match (skip_reward_cert, notar_reward_cert) {
        (None, None) => None,

        (Some(skip), None) => {
            let rank_map = get_rank_map(bank, skip.slot)?;
            let mut reward_validators = HashSet::with_capacity(rank_map.len());
            let vote = Vote::new_skip_vote(skip.slot);
            let message = get_vote_payload_to_sign(vote, shred_version);
            let (pubkeys, _) = get_pubkeys(rank_map, skip.to_bitmap(), &mut reward_validators)?;
            let payload = Payload {
                signature: &skip.signature,
                pubkeys,
                message,
            };
            payloads.push(payload);
            Some((skip.slot, reward_validators))
        }

        (None, Some(notar)) => {
            let rank_map = get_rank_map(bank, notar.slot)?;
            let mut reward_validators = HashSet::with_capacity(rank_map.len());
            let vote = Vote::new_notarization_vote(Block {
                slot: notar.slot,
                block_id: BlockId::from(notar.block_id),
            });
            let message = get_vote_payload_to_sign(vote, shred_version);
            let (pubkeys, _) = get_pubkeys(rank_map, notar.bitmap(), &mut reward_validators)?;
            let payload = Payload {
                signature: &notar.signature,
                pubkeys,
                message,
            };
            payloads.push(payload);
            Some((notar.slot, reward_validators))
        }

        (Some(skip), Some(notar)) => {
            if skip.slot != notar.slot {
                return Err(ValidatedBlockCertsError::RewardSlotMismatch);
            }
            let rank_map = get_rank_map(bank, notar.slot)?;
            let mut reward_validators = HashSet::with_capacity(rank_map.len());
            {
                let vote = Vote::new_skip_vote(skip.slot);
                let message = get_vote_payload_to_sign(vote, shred_version);
                let (pubkeys, _) = get_pubkeys(rank_map, skip.to_bitmap(), &mut reward_validators)?;
                let payload = Payload {
                    signature: &skip.signature,
                    pubkeys,
                    message,
                };
                payloads.push(payload);
            }
            {
                let vote = Vote::new_notarization_vote(Block {
                    slot: notar.slot,
                    block_id: BlockId::from(notar.block_id),
                });
                let message = get_vote_payload_to_sign(vote, shred_version);
                let (pubkeys, _) = get_pubkeys(rank_map, notar.bitmap(), &mut reward_validators)?;
                let payload = Payload {
                    signature: &notar.signature,
                    pubkeys,
                    message,
                };
                payloads.push(payload);
            }
            Some((skip.slot, reward_validators))
        }
    };
    Ok(ret)
}

fn get_final_payloads<'a>(
    bank: &Bank,
    shred_version: u16,
    block_cert: &'a Option<BlockFinalizationCert>,
    payloads: &mut Vec<Payload<'a>>,
) -> Result<HashSet<Pubkey>, ValidatedBlockCertsError> {
    let mut validators = HashSet::new();
    if let Some(block_cert) = &block_cert {
        let block = Block {
            slot: block_cert.slot,
            block_id: block_cert.block_id,
        };
        let rank_map = get_rank_map(bank, block_cert.slot)?;
        let notar_vote = Vote::new_notarization_vote(block);

        match &block_cert.notar_aggregate {
            None => {
                let message = get_vote_payload_to_sign(notar_vote, shred_version);
                let (signature, bitmap) = block_cert.final_aggregate.as_parts();
                let (pubkeys, cert_stake) = get_pubkeys(rank_map, bitmap, &mut validators)?;
                let threshold = CertificateType::FinalizeFast(block).threshold();
                if Fraction::new(cert_stake.0, rank_map.total_stake()) < threshold {
                    return Err(ValidatedBlockCertsError::StakeThresholdNotMet);
                }
                let payload = Payload {
                    signature,
                    pubkeys,
                    message,
                };
                payloads.push(payload);
            }
            Some(notar_aggregate) => {
                let vote = Vote::new_finalization_vote(block_cert.slot);
                let message = get_vote_payload_to_sign(vote, shred_version);
                let (signature, bitmap) = block_cert.final_aggregate.as_parts();
                let (pubkeys, cert_stake) = get_pubkeys(rank_map, bitmap, &mut validators)?;
                let threshold = CertificateType::Notarize(block).threshold();
                if Fraction::new(cert_stake.0, rank_map.total_stake()) < threshold {
                    return Err(ValidatedBlockCertsError::StakeThresholdNotMet);
                }
                let payload = Payload {
                    signature,
                    pubkeys,
                    message,
                };
                payloads.push(payload);

                let message = get_vote_payload_to_sign(notar_vote, shred_version);
                let (signature, bitmap) = notar_aggregate.as_parts();
                let (pubkeys, cert_stake) = get_pubkeys(rank_map, bitmap, &mut validators)?;
                let threshold = CertificateType::Finalize(block_cert.slot).threshold();
                if Fraction::new(cert_stake.0, rank_map.total_stake()) < threshold {
                    return Err(ValidatedBlockCertsError::StakeThresholdNotMet);
                }
                let payload = Payload {
                    signature,
                    pubkeys,
                    message,
                };
                payloads.push(payload);
            }
        }
    }
    Ok(validators)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            bank_forks::BankForks,
            genesis_utils::{
                ValidatorVoteKeypairs, create_genesis_config_with_tower_vote_accounts,
                create_genesis_config_with_vote_accounts,
            },
        },
        agave_votor_messages::{
            certificate::{Certificate, GenesisCert},
            reward_certificate::NUM_SLOTS_FOR_REWARD,
        },
        bitvec::prelude::*,
        rand::Rng,
        rayon::ThreadPoolBuilder,
        solana_bls_signatures::{BLS_SIGNATURE_COMPRESSED_SIZE, SignatureProjective},
        solana_entry::block_component::VotesAggregate,
        solana_leader_schedule::SlotLeader,
        solana_signer_store::{encode_base2, encode_base3},
        std::sync::{Arc, RwLock},
    };

    type TestBank = (
        Arc<Bank>,
        Vec<ValidatorVoteKeypairs>,
        Arc<RwLock<BankForks>>,
    );

    /// Creates a bank with BLS-enabled validators for testing certificate verification.
    /// Keeps the bank forks alive alongside the bank and its validator keypairs.
    fn create_bank_with_bls_validators(num_validators: usize, stakes: Vec<u64>) -> TestBank {
        assert_eq!(num_validators, stakes.len());
        let validator_keypairs: Vec<ValidatorVoteKeypairs> = (0..num_validators)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect();

        let genesis_config_info =
            create_genesis_config_with_vote_accounts(10_000_000, &validator_keypairs, stakes);

        let (bank, bank_forks) = Bank::new_for_tests(&genesis_config_info.genesis_config)
            .wrap_with_bank_forks_for_tests();
        (bank, validator_keypairs, bank_forks)
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

    fn create_reward_bank(migration_slot: Slot, reward_slot: Slot) -> TestBank {
        let (root_bank, validator_keypairs, bank_forks) =
            create_bank_with_bls_validators(8, vec![800, 700, 600, 500, 400, 300, 200, 100]);
        let bank = Arc::new(Bank::new_from_parent(
            root_bank,
            SlotLeader::default(),
            reward_slot + NUM_SLOTS_FOR_REWARD,
        ));
        let genesis_block = Block::new_unique(migration_slot);
        bank.set_alpenglow_genesis_certificate(&GenesisCert {
            block: genesis_block,
            signature: build_cert_signature(
                Vote::new_genesis_vote(genesis_block),
                0,
                &[0],
                &validator_keypairs,
            ),
        });
        (bank, validator_keypairs, bank_forks)
    }

    fn build_reward_certs(
        block: Block,
        shred_version: u16,
        validator_keypairs: &[ValidatorVoteKeypairs],
    ) -> (SkipRewardCertificate, NotarRewardCertificate) {
        let skip_signature = build_cert_signature(
            Vote::new_skip_vote(block.slot),
            shred_version,
            &[5, 6, 7],
            validator_keypairs,
        );
        let notar_signature = build_cert_signature(
            Vote::new_notarization_vote(block),
            shred_version,
            &[0, 1, 2, 3, 4],
            validator_keypairs,
        );
        (
            SkipRewardCertificate::try_new(
                block.slot,
                skip_signature.signature.try_into().unwrap(),
                skip_signature.bitmap,
            )
            .unwrap(),
            NotarRewardCertificate::try_new(
                block.slot,
                block.block_id.to_hash(),
                notar_signature.signature.try_into().unwrap(),
                notar_signature.bitmap,
            )
            .unwrap(),
        )
    }

    #[derive(Clone, Copy, Debug)]
    enum AggregateKind {
        SkipReward,
        NotarReward,
        FastFinalize,
        Finalize,
        Notarize,
    }

    const AGGREGATE_KINDS: [AggregateKind; 5] = [
        AggregateKind::SkipReward,
        AggregateKind::NotarReward,
        AggregateKind::FastFinalize,
        AggregateKind::Finalize,
        AggregateKind::Notarize,
    ];

    /// Replaces one aggregate while leaving every other input valid.
    fn verify_modified_aggregate(
        bank: &Bank,
        validator_keypairs: &[ValidatorVoteKeypairs],
        shred_version: u16,
        kind: AggregateKind,
        replace: impl FnOnce(Vote, SignatureCompressed, Vec<u8>) -> (SignatureCompressed, Vec<u8>),
    ) -> Result<(), ValidatedBlockCertsError> {
        let thread_pool = ThreadPoolBuilder::new().num_threads(2).build().unwrap();
        let reward_slot = bank.slot() - NUM_SLOTS_FOR_REWARD;
        let block = Block::new_unique(bank.slot() - 1);
        let reward_block = Block::new_unique(reward_slot);
        let vote = match kind {
            AggregateKind::SkipReward => Vote::new_skip_vote(reward_slot),
            AggregateKind::NotarReward => Vote::new_notarization_vote(reward_block),
            AggregateKind::FastFinalize | AggregateKind::Notarize => {
                Vote::new_notarization_vote(block)
            }
            AggregateKind::Finalize => Vote::new_finalization_vote(block.slot),
        };
        let ranks: Vec<_> = (0..validator_keypairs.len()).collect();
        let signature = build_cert_signature(vote, shred_version, &ranks, validator_keypairs);
        let (signature, bitmap) = replace(
            vote,
            signature.signature.try_into().unwrap(),
            signature.bitmap,
        );
        let (block_cert, skip, notar) = match kind {
            AggregateKind::SkipReward => (
                None,
                Some(SkipRewardCertificate::try_new(reward_slot, signature, bitmap).unwrap()),
                None,
            ),
            AggregateKind::NotarReward => (
                None,
                None,
                Some(
                    NotarRewardCertificate::try_new(
                        reward_slot,
                        reward_block.block_id.to_hash(),
                        signature,
                        bitmap,
                    )
                    .unwrap(),
                ),
            ),
            AggregateKind::FastFinalize | AggregateKind::Finalize | AggregateKind::Notarize => {
                let modified = VotesAggregate::new(signature, bitmap);
                let (final_aggregate, notar_aggregate) = match kind {
                    AggregateKind::FastFinalize => (modified, None),
                    AggregateKind::Finalize => (
                        modified,
                        Some(VotesAggregate::from_cert_signature(build_cert_signature(
                            Vote::new_notarization_vote(block),
                            shred_version,
                            &ranks,
                            validator_keypairs,
                        ))),
                    ),
                    AggregateKind::Notarize => (
                        VotesAggregate::from_cert_signature(build_cert_signature(
                            Vote::new_finalization_vote(block.slot),
                            shred_version,
                            &ranks,
                            validator_keypairs,
                        )),
                        Some(modified),
                    ),
                    _ => unreachable!(),
                };
                (
                    Some(BlockFinalizationCert {
                        slot: block.slot,
                        block_id: block.block_id,
                        final_aggregate,
                        notar_aggregate,
                    }),
                    None,
                    None,
                )
            }
        };
        verify_certs(&thread_pool, bank, shred_version, block_cert, skip, notar).map(|_| ())
    }

    #[test]
    fn test_verify_certs_reject_malformed_compressed_signatures() {
        let (bank, validator_keypairs, _bank_forks) = create_reward_bank(0, 1);
        for kind in AGGREGATE_KINDS {
            // Confirm that only the replacement makes the certificate invalid.
            verify_modified_aggregate(&bank, &validator_keypairs, 42, kind, |_, s, b| (s, b))
                .unwrap();
            for bytes in [
                [0; BLS_SIGNATURE_COMPRESSED_SIZE],
                [u8::MAX; BLS_SIGNATURE_COMPRESSED_SIZE],
            ] {
                let signature = SignatureCompressed(bytes);
                assert!(signature.try_as_affine().is_err());
                let err = verify_modified_aggregate(
                    &bank,
                    &validator_keypairs,
                    42,
                    kind,
                    |_, _, bitmap| (signature, bitmap),
                )
                .unwrap_err();
                assert!(
                    matches!(err, ValidatedBlockCertsError::SignatureVerificationFailed),
                    "{kind:?}: {err:?}"
                );
            }
        }
    }

    #[test]
    fn test_verify_certs_reject_malformed_and_out_of_range_bitmaps() {
        let (bank, validator_keypairs, _bank_forks) = create_reward_bank(0, 1);
        let mut out_of_range = BitVec::<u8, Lsb0>::repeat(false, validator_keypairs.len() + 1);
        out_of_range.set(validator_keypairs.len(), true);
        for bitmap in [
            vec![],              // Missing header.
            vec![u8::MAX, 0, 0], // Unsupported encoding.
            vec![0, 8, 0],       // Missing base-2 data.
            encode_base2(&out_of_range).unwrap(),
        ] {
            for kind in AGGREGATE_KINDS {
                let err =
                    verify_modified_aggregate(&bank, &validator_keypairs, 42, kind, |_, s, _| {
                        (s, bitmap.clone())
                    })
                    .unwrap_err();
                assert!(
                    matches!(err, ValidatedBlockCertsError::DecodeFailed(_)),
                    "{kind:?}: {err:?}"
                );
            }
        }
    }

    #[test]
    fn test_verify_certs_reject_base3_bitmaps() {
        let (bank, validator_keypairs, _bank_forks) = create_reward_bank(0, 1);
        let mut primary = BitVec::<u8, Lsb0>::repeat(false, validator_keypairs.len());
        let mut fallback = primary.clone();
        primary.set(0, true);
        fallback.set(1, true);
        let bitmap = encode_base3(&primary, &fallback).unwrap();
        for kind in AGGREGATE_KINDS {
            let err = verify_modified_aggregate(&bank, &validator_keypairs, 42, kind, |_, s, _| {
                (s, bitmap.clone())
            })
            .unwrap_err();
            assert!(
                matches!(err, ValidatedBlockCertsError::Base3Cert),
                "{kind:?}: {err:?}"
            );
        }
    }

    #[test]
    fn test_verify_certs_reject_empty_signer_sets() {
        let (bank, validator_keypairs, _bank_forks) = create_reward_bank(0, 1);
        for len in [0, validator_keypairs.len()] {
            let bitmap = encode_base2(&BitVec::<u8, Lsb0>::repeat(false, len)).unwrap();
            for kind in AGGREGATE_KINDS {
                let err =
                    verify_modified_aggregate(&bank, &validator_keypairs, 42, kind, |_, s, _| {
                        (s, bitmap.clone())
                    })
                    .unwrap_err();
                match kind {
                    AggregateKind::SkipReward | AggregateKind::NotarReward => assert!(
                        matches!(err, ValidatedBlockCertsError::PubkeyAggregationFailed),
                        "{kind:?}: {err:?}",
                    ),
                    _ => assert!(
                        matches!(err, ValidatedBlockCertsError::StakeThresholdNotMet),
                        "{kind:?}: {err:?}"
                    ),
                }
            }
        }
    }

    #[test]
    fn test_verify_certs_reject_wrong_shred_version() {
        let (bank, validator_keypairs, _bank_forks) = create_reward_bank(0, 1);
        for kind in AGGREGATE_KINDS {
            let err = verify_modified_aggregate(
                &bank,
                &validator_keypairs,
                42,
                kind,
                |vote, _, bitmap| {
                    let signature = build_cert_signature(
                        vote,
                        43,
                        &(0..8).collect::<Vec<_>>(),
                        &validator_keypairs,
                    );
                    (signature.signature.try_into().unwrap(), bitmap)
                },
            );
            assert!(
                matches!(
                    err,
                    Err(ValidatedBlockCertsError::SignatureVerificationFailed)
                ),
                "{kind:?}: {err:?}"
            );
        }
    }

    fn build_block_cert(
        block: Block,
        shred_version: u16,
        validator_keypairs: &[ValidatorVoteKeypairs],
        final_ranks: &[usize],
        notar_ranks: Option<&[usize]>,
    ) -> BlockFinalizationCert {
        let final_vote = if notar_ranks.is_some() {
            Vote::new_finalization_vote(block.slot)
        } else {
            Vote::new_notarization_vote(block)
        };
        BlockFinalizationCert {
            slot: block.slot,
            block_id: block.block_id,
            final_aggregate: VotesAggregate::from_cert_signature(build_cert_signature(
                final_vote,
                shred_version,
                final_ranks,
                validator_keypairs,
            )),
            notar_aggregate: notar_ranks.map(|ranks| {
                VotesAggregate::from_cert_signature(build_cert_signature(
                    Vote::new_notarization_vote(block),
                    shred_version,
                    ranks,
                    validator_keypairs,
                ))
            }),
        }
    }

    #[test]
    fn test_verify_final_certs_at_exact_stake_thresholds() {
        let thread_pool = ThreadPoolBuilder::new().num_threads(2).build().unwrap();
        // Total stake is 10,000; these ranks distinguish the threshold from one lamport below it.
        let (bank, validator_keypairs, _bank_forks) =
            create_bank_with_bls_validators(4, vec![4000, 3999, 2000, 1]);
        let block = Block::new_unique(bank.slot());
        for (final_ranks, notar_ranks, valid) in [
            (vec![0, 1, 3], None, true),           // Exactly 80%.
            (vec![0, 1], None, false),             // 80% minus one lamport.
            (vec![0, 2], Some(vec![0, 2]), true),  // Exactly 60% for both.
            (vec![1, 2], Some(vec![0, 2]), false), // Finalize just below 60%.
            (vec![0, 2], Some(vec![1, 2]), false), // Notarize just below 60%.
        ] {
            let cert = build_block_cert(
                block,
                42,
                &validator_keypairs,
                &final_ranks,
                notar_ranks.as_deref(),
            );
            let result = verify_certs(&thread_pool, &bank, 42, Some(cert), None, None);
            if valid {
                let (reward, final_cert) = result.unwrap();
                assert!(reward.is_none());
                assert_eq!(final_cert.unwrap().block(), block);
            } else {
                assert!(matches!(
                    result,
                    Err(ValidatedBlockCertsError::StakeThresholdNotMet)
                ));
            }
        }
    }

    #[test]
    fn test_verify_reward_certs_deduplicate_overlapping_signers() {
        let thread_pool = ThreadPoolBuilder::new().num_threads(2).build().unwrap();
        let (bank, validator_keypairs, _bank_forks) = create_reward_bank(0, 1);
        let block = Block::new_unique(1);
        let (_, notar) = build_reward_certs(block, 42, &validator_keypairs);
        let signature =
            build_cert_signature(Vote::new_skip_vote(1), 42, &[3, 4, 5], &validator_keypairs);
        let skip = SkipRewardCertificate::try_new(
            1,
            signature.signature.try_into().unwrap(),
            signature.bitmap,
        )
        .unwrap();
        let (reward, final_cert) =
            verify_certs(&thread_pool, &bank, 42, None, Some(skip), Some(notar)).unwrap();
        assert!(final_cert.is_none());
        let reward = reward.unwrap();
        assert_eq!(reward.slot(), 1);
        assert_eq!(reward.validators().len(), 6);
        assert_eq!(
            reward.validators(),
            &vote_pubkeys_for_ranks(&bank, &[0, 1, 2, 3, 4, 5])
        );
    }

    #[test]
    fn test_verify_reward_certs_reject_missing_migration_slot() {
        let thread_pool = ThreadPoolBuilder::new().num_threads(2).build().unwrap();
        // The regular genesis helper installs a migration certificate at slot zero.
        let validator_keypairs: Vec<_> =
            (0..8).map(|_| ValidatorVoteKeypairs::new_rand()).collect();
        let genesis = create_genesis_config_with_tower_vote_accounts(
            10_000_000,
            &validator_keypairs,
            vec![800, 700, 600, 500, 400, 300, 200, 100],
        );
        let (root_bank, _bank_forks) =
            Bank::new_for_tests(&genesis.genesis_config).wrap_with_bank_forks_for_tests();
        let bank =
            Bank::new_from_parent(root_bank, SlotLeader::default(), 1 + NUM_SLOTS_FOR_REWARD);
        assert_eq!(bank.get_alpenglow_migration_slot(), None);
        let (skip, notar) = build_reward_certs(Block::new_unique(1), 42, &validator_keypairs);
        for (skip, notar) in [
            (Some(skip.clone()), None),
            (None, Some(notar.clone())),
            (Some(skip), Some(notar)),
        ] {
            let expected = InvalidSlots {
                current_slot: bank.slot(),
                skip_slot: skip.as_ref().map(|c| c.slot),
                notar_slot: notar.as_ref().map(|c| c.slot),
            };
            let err = verify_certs(&thread_pool, &bank, 42, None, skip, notar).unwrap_err();
            let ValidatedBlockCertsError::InvalidRewardSlots(actual) = err else {
                panic!("unexpected error: {err:?}");
            };
            assert_eq!(actual, expected);
        }
        let (reward, final_cert) = verify_certs(&thread_pool, &bank, 42, None, None, None).unwrap();
        assert!(reward.is_none());
        assert!(final_cert.is_none());
    }

    #[test]
    fn test_verify_certs_use_each_slots_epoch_stakes() {
        let thread_pool = ThreadPoolBuilder::new().num_threads(2).build().unwrap();
        let (old_bank, old_keypairs, _old_bank_forks) =
            create_bank_with_bls_validators(8, vec![800, 700, 600, 500, 400, 300, 200, 100]);
        let boundary = old_bank.epoch_schedule().get_first_slot_in_epoch(1);
        let reward_slot = boundary - 1;
        let (mut bank, new_keypairs, _bank_forks) = create_reward_bank(0, reward_slot);
        let final_slot = bank.slot() - 1;
        assert_eq!(bank.epoch_schedule().get_epoch(reward_slot), 0);
        assert_eq!(bank.epoch_schedule().get_epoch(final_slot), 1);
        let old_stakes = old_bank.epoch_stakes(0).unwrap().clone();
        Arc::get_mut(&mut bank)
            .unwrap()
            .set_epoch_stakes_for_test(0, old_stakes);

        let old_signers = vote_pubkeys_for_ranks(&old_bank, &(0..8).collect::<Vec<_>>());
        let new_signers = vote_pubkeys_for_ranks(&bank, &(0..8).collect::<Vec<_>>());
        assert!(old_signers.is_disjoint(&new_signers));
        let block = Block::new_unique(final_slot);
        for notar_ranks in [None, Some((0..8).collect::<Vec<_>>())] {
            let cert = build_block_cert(
                block,
                42,
                &new_keypairs,
                &(0..8).collect::<Vec<_>>(),
                notar_ranks.as_deref(),
            );
            let (skip, notar) =
                build_reward_certs(Block::new_unique(reward_slot), 42, &old_keypairs);
            let (reward, final_cert) =
                verify_certs(&thread_pool, &bank, 42, Some(cert), Some(skip), Some(notar)).unwrap();
            let reward = reward.unwrap();
            assert_eq!(reward.slot(), reward_slot);
            assert_eq!(reward.validators(), &old_signers);
            let final_cert = final_cert.unwrap();
            assert_eq!(final_cert.block(), block);
            assert_eq!(final_cert.vote_rewards_input().0, &new_signers);

            // Signing either certificate with the other epoch's keys must fail.
            let wrong_final = build_block_cert(
                block,
                42,
                &old_keypairs,
                &(0..8).collect::<Vec<_>>(),
                notar_ranks.as_deref(),
            );
            assert!(matches!(
                verify_certs(&thread_pool, &bank, 42, Some(wrong_final), None, None),
                Err(ValidatedBlockCertsError::SignatureVerificationFailed)
            ));
            let (skip, notar) =
                build_reward_certs(Block::new_unique(reward_slot), 42, &new_keypairs);
            assert!(matches!(
                verify_certs(&thread_pool, &bank, 42, None, Some(skip), Some(notar)),
                Err(ValidatedBlockCertsError::SignatureVerificationFailed)
            ));
        }
    }

    #[test]
    fn test_verify_reward_certs_valid() {
        let thread_pool = ThreadPoolBuilder::new().num_threads(2).build().unwrap();
        let reward_slot = 1;
        let (bank, validator_keypairs, _bank_forks) = create_reward_bank(0, reward_slot);
        let shred_version = rand::rng().random();
        let (skip, notar) = build_reward_certs(
            Block::new_unique(reward_slot),
            shred_version,
            &validator_keypairs,
        );

        for (skip, notar, ranks) in [
            (None, None, vec![]),
            (Some(skip.clone()), None, vec![5, 6, 7]),
            (None, Some(notar.clone()), vec![0, 1, 2, 3, 4]),
            (Some(skip), Some(notar), (0..8).collect()),
        ] {
            let (reward, final_cert) =
                verify_certs(&thread_pool, &bank, shred_version, None, skip, notar).unwrap();
            assert!(final_cert.is_none());
            if ranks.is_empty() {
                assert!(reward.is_none());
            } else {
                let reward = reward.unwrap();
                assert_eq!(reward.slot(), reward_slot);
                assert_eq!(reward.validators(), &vote_pubkeys_for_ranks(&bank, &ranks));
            }
        }
    }

    #[test]
    fn test_verify_reward_and_final_certs_together() {
        let thread_pool = ThreadPoolBuilder::new().num_threads(2).build().unwrap();
        let reward_slot = 1;
        let (bank, validator_keypairs, _bank_forks) = create_reward_bank(0, reward_slot);
        let shred_version = rand::rng().random();
        let (skip, notar) = build_reward_certs(
            Block::new_unique(reward_slot),
            shred_version,
            &validator_keypairs,
        );
        let block = Block::new_unique(bank.slot() - 1);
        let final_signature = build_cert_signature(
            Vote::new_finalization_vote(block.slot),
            shred_version,
            &[0, 1, 2, 3],
            &validator_keypairs,
        );
        let notar_signature = build_cert_signature(
            Vote::new_notarization_vote(block),
            shred_version,
            &[0, 1, 2, 4],
            &validator_keypairs,
        );
        let block_cert = BlockFinalizationCert {
            slot: block.slot,
            block_id: block.block_id,
            final_aggregate: VotesAggregate::from_cert_signature(final_signature),
            notar_aggregate: Some(VotesAggregate::from_cert_signature(notar_signature)),
        };

        let (reward, final_cert) = verify_certs(
            &thread_pool,
            &bank,
            shred_version,
            Some(block_cert),
            Some(skip.clone()),
            Some(notar.clone()),
        )
        .unwrap();
        let reward = reward.unwrap();
        assert_eq!(reward.slot(), reward_slot);
        assert_eq!(
            reward.validators(),
            &vote_pubkeys_for_ranks(&bank, &(0..8).collect::<Vec<_>>())
        );
        let final_cert = final_cert.unwrap();
        assert_eq!(final_cert.block(), block);
        assert_eq!(
            final_cert.vote_rewards_input().0,
            &vote_pubkeys_for_ranks(&bank, &[0, 1, 2, 3, 4])
        );

        // Every payload must verify before either validated certificate is returned.
        for corrupted_payload in 0..4 {
            let mut block_cert = final_cert.to_block_final_cert();
            let mut skip = skip.clone();
            let mut notar = notar.clone();
            match corrupted_payload {
                0 => skip.signature = notar.signature,
                1 => notar.block_id = BlockId::new_unique().to_hash(),
                2 => {
                    block_cert.final_aggregate =
                        VotesAggregate::from_cert_signature(build_cert_signature(
                            Vote::new_finalization_vote(block.slot + 1),
                            shred_version,
                            &[0, 1, 2, 3],
                            &validator_keypairs,
                        ))
                }
                3 => block_cert.block_id = BlockId::new_unique(),
                _ => unreachable!(),
            }
            let err = verify_certs(
                &thread_pool,
                &bank,
                shred_version,
                Some(block_cert),
                Some(skip),
                Some(notar),
            )
            .unwrap_err();
            assert!(matches!(
                err,
                ValidatedBlockCertsError::SignatureVerificationFailed
            ));
        }
    }

    #[test]
    fn test_verify_reward_certs_reject_invalid_slots() {
        let thread_pool = ThreadPoolBuilder::new().num_threads(2).build().unwrap();
        let reward_slot = 2;
        let (bank, validator_keypairs, _bank_forks) = create_reward_bank(1, reward_slot);
        let shred_version = rand::rng().random();
        for slot in [0, 1, reward_slot, reward_slot + 1] {
            let (skip, notar) =
                build_reward_certs(Block::new_unique(slot), shred_version, &validator_keypairs);
            for (skip, notar) in [
                (Some(skip.clone()), None),
                (None, Some(notar.clone())),
                (Some(skip.clone()), Some(notar.clone())),
            ] {
                let result = verify_certs(&thread_pool, &bank, shred_version, None, skip, notar);
                if slot == reward_slot {
                    assert_eq!(result.unwrap().0.unwrap().slot(), reward_slot);
                } else {
                    assert!(matches!(
                        result,
                        Err(ValidatedBlockCertsError::InvalidRewardSlots(_))
                    ));
                }
            }
        }
        let (skip, _) = build_reward_certs(
            Block::new_unique(reward_slot),
            shred_version,
            &validator_keypairs,
        );
        let (_, notar) = build_reward_certs(
            Block::new_unique(reward_slot + 1),
            shred_version,
            &validator_keypairs,
        );
        assert!(matches!(
            verify_certs(
                &thread_pool,
                &bank,
                shred_version,
                None,
                Some(skip),
                Some(notar)
            ),
            Err(ValidatedBlockCertsError::InvalidRewardSlots(_))
        ));

        // Even correctly aged, signed certificates cannot reward Tower slots.
        for migration_slot in [reward_slot, reward_slot + 1] {
            let (bank, validator_keypairs, _bank_forks) =
                create_reward_bank(migration_slot, reward_slot);
            let (skip, notar) = build_reward_certs(
                Block::new_unique(reward_slot),
                shred_version,
                &validator_keypairs,
            );
            for (skip, notar) in [
                (Some(skip.clone()), None),
                (None, Some(notar.clone())),
                (Some(skip), Some(notar)),
            ] {
                assert!(matches!(
                    verify_certs(&thread_pool, &bank, shred_version, None, skip, notar),
                    Err(ValidatedBlockCertsError::InvalidRewardSlots(_))
                ));
            }
        }
    }

    #[test]
    fn test_verify_final_cert_valid() {
        let thread_pool = ThreadPoolBuilder::new().num_threads(2).build().unwrap();
        // Create 10 validators with descending stakes (1000, 900, 800, ...)
        // Total stake = 5500
        let num_validators = 10;
        let stakes: Vec<u64> = (0..num_validators)
            .map(|i| (1000u64).saturating_sub((i as u64).saturating_mul(100)))
            .collect();
        let (bank, validator_keypairs, _bank_forks) =
            create_bank_with_bls_validators(num_validators, stakes);
        let shred_version = rand::rng().random();

        let block = Block::new_unique(bank.slot());

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

            let validated = verify_certs(
                &thread_pool,
                &bank,
                shred_version,
                Some(block_final_cert),
                None,
                None,
            )
            .expect("Valid fast finalize certificate should pass verification");
            assert!(validated.0.is_none());
            let validated = validated.1.unwrap();
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

            let validated = verify_certs(
                &thread_pool,
                &bank,
                shred_version,
                Some(block_final_cert),
                None,
                None,
            )
            .expect("Valid slow finalize certificate should pass verification");
            assert!(validated.0.is_none());
            let validated = validated.1.unwrap();
            assert_eq!(
                validated.clone_certificates(),
                vec![finalize_cert, notarize_cert]
            );
        }
    }

    #[test]
    fn test_slow_finalization_signers_include_notarize_and_finalize_signers() {
        let thread_pool = ThreadPoolBuilder::new().num_threads(2).build().unwrap();
        let stakes = vec![1000, 900, 800, 700, 100];
        let (bank, validator_keypairs, _bank_forks) =
            create_bank_with_bls_validators(stakes.len(), stakes);
        let shred_version = rand::rng().random();

        let block = Block::new_unique(bank.slot());

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
        let validated = verify_certs(
            &thread_pool,
            &bank,
            shred_version,
            Some(block_final_cert),
            None,
            None,
        )
        .expect("Valid slow finalize certificate should pass verification");
        assert!(validated.0.is_none());
        let validated = validated.1.unwrap();
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
        let thread_pool = ThreadPoolBuilder::new().num_threads(2).build().unwrap();
        // Create 10 validators with descending stakes (1000, 900, 800, ...)
        // Total stake = 5500
        let num_validators = 10;
        let stakes: Vec<u64> = (0..num_validators)
            .map(|i| (1000u64).saturating_sub((i as u64).saturating_mul(100)))
            .collect();
        let (bank, validator_keypairs, _bank_forks) =
            create_bank_with_bls_validators(num_validators, stakes);
        let shred_version = rand::rng().random();

        let block = Block::new_unique(bank.slot());

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

            let err = verify_certs(
                &thread_pool,
                &bank,
                shred_version,
                Some(block_final_cert),
                None,
                None,
            )
            .unwrap_err();
            assert!(
                matches!(err, ValidatedBlockCertsError::StakeThresholdNotMet),
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

            let err = verify_certs(
                &thread_pool,
                &bank,
                shred_version,
                Some(block_final_cert),
                None,
                None,
            )
            .unwrap_err();
            assert!(
                matches!(err, ValidatedBlockCertsError::StakeThresholdNotMet),
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

            let err = verify_certs(
                &thread_pool,
                &bank,
                shred_version,
                Some(block_final_cert),
                None,
                None,
            )
            .unwrap_err();
            assert!(
                matches!(err, ValidatedBlockCertsError::StakeThresholdNotMet),
                "Slow finalize with insufficient finalize stake should fail verification"
            );
        }
    }
}
