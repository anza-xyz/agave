#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    agave_votor_messages::{
        certificate::Certificate, fraction::Fraction,
        unverified_vote_message::UnverifiedCertificate,
    },
    bitvec::vec::BitVec,
    rayon::iter::IntoParallelRefIterator,
    solana_bls_signatures::{
        BlsError, PubkeyProjective, Signature as BlsSignature, SignatureProjective,
        VerifySignature,
        pubkey::{AggregatePubkey, PopVerified, PubkeyAffine as BlsPubkeyAffine},
        signature::AsSignatureAffine,
    },
    solana_signer_store::{DecodeError, Decoded, decode},
    std::num::NonZero,
    thiserror::Error,
};

/// Minimum size of the rayon thread pool required for this crate to use the thread pool.
///  Otherwise, the operations will be done sequentially on the current thread.
const THREAD_POOL_THRESHOLD: usize = 4;

#[derive(Debug, PartialEq, Eq, Error)]
pub enum Error {
    #[error("missing rank in rank map")]
    MissingRank,
    #[error("missing rank map")]
    MissingRankMap,
    #[error("verify signature failed with {0:?}")]
    VerifySig(#[from] BlsError),
    #[error("decoding bitmap failed with {0:?}")]
    Decode(DecodeError),
    #[error("wrong encoding base")]
    WrongEncoding,
    #[error("overlapping primary and fallback bitmaps")]
    BitmapOverlap,
    #[error("Not enough stake {aggregate_stake}: {cert_fraction} < {required_fraction}")]
    NotEnoughStake {
        aggregate_stake: u64,
        cert_fraction: Fraction,
        required_fraction: Fraction,
    },
}

/// Verifies an Alpenglow `UnverifiedCertificate`.
///
/// This function verifies that the aggregate signature matches the vote payload(s)
/// derived from the certificate type.
///
/// # Core Functionality
/// - **Signature Verification**: Validates aggregate BLS signatures for `votor-messages`.
/// - **Bitmap Processing**: Supports verification using bitmaps that conform to the
///   `solana-signer-store` format.
/// - **Stake verification**: Verifies that sufficient stake signed the certificate.
///
/// # Ledger State and Rank Map
///
/// To maintain a lightweight dependency graph, this function does not manage the ledger
/// state directly. Instead, the stake distribution and validator public keys must be
/// provided via the `rank_map` lookup function.
///
/// # Arguments
///
/// * `cert` - The certificate to verify.
/// * `max_validators` - The maximum number of validators (used for decoding the bitmap).
/// * `total_stake` - The total stake for the network for the epoch in which this certificate was produced.
/// * `rank_map` - A closure that maps a validator's rank (index) to their stake and public key.
pub fn verify_certificate(
    cert: UnverifiedCertificate,
    max_validators: usize,
    total_stake: NonZero<u64>,
    mut rank_map: impl FnMut(usize) -> Option<(NonZero<u64>, PopVerified<BlsPubkeyAffine>)>,
) -> Result<Certificate, Error> {
    let mut aggregate_stake = 0u64;

    // Wrap the `rank_map` to accumulate stake as a side-effect
    let accumulating_rank_map = |ind: usize| {
        rank_map(ind).map(|(stake, pubkey)| {
            aggregate_stake = aggregate_stake.saturating_add(stake.get());
            pubkey
        })
    };

    let (primary_payload, fallback_payload) = cert.cert_type.get_vote_payload(cert.shred_version);

    if let Some(fallback_payload) = fallback_payload {
        verify_base3(
            &primary_payload,
            &fallback_payload,
            &cert.signature,
            &cert.bitmap,
            max_validators,
            accumulating_rank_map,
        )
    } else {
        verify_base2(
            &primary_payload,
            &cert.signature,
            &cert.bitmap,
            max_validators,
            accumulating_rank_map,
        )
    }?;
    verify_stake(&cert, aggregate_stake, total_stake)?;
    Ok(Certificate {
        cert_type: cert.cert_type,
        signature: cert.signature,
        bitmap: cert.bitmap,
    })
}

fn verify_stake(
    cert: &UnverifiedCertificate,
    aggregate_stake: u64,
    total_stake: NonZero<u64>,
) -> Result<(), Error> {
    let required_fraction = cert.cert_type.threshold();
    let cert_fraction = Fraction::new(aggregate_stake, total_stake);
    if cert_fraction >= required_fraction {
        Ok(())
    } else {
        Err(Error::NotEnoughStake {
            aggregate_stake,
            cert_fraction,
            required_fraction,
        })
    }
}

/// Verifies a signature for a single payload signed by a set of validators.
///
/// This function handles the "Base2" case where all participating validators have signed
/// the exact same payload. This is the standard verification path and covers virtually all
/// cases in practice.
pub fn verify_base2<S: AsSignatureAffine>(
    payload: &[u8],
    signature: &S,
    ranks: &[u8],
    max_validators: usize,
    rank_map: impl FnMut(usize) -> Option<PopVerified<BlsPubkeyAffine>>,
) -> Result<(), Error> {
    let ranks = decode(ranks, max_validators).map_err(Error::Decode)?;
    let ranks = match ranks {
        Decoded::Base2(ranks) => ranks,
        Decoded::Base3(_, _) => return Err(Error::WrongEncoding),
    };
    verify_single_vote_signature(payload, signature, &ranks, rank_map)
}

fn verify_single_vote_signature<S: AsSignatureAffine>(
    payload: &[u8],
    signature: &S,
    ranks: &BitVec<u8>,
    rank_map: impl FnMut(usize) -> Option<PopVerified<BlsPubkeyAffine>>,
) -> Result<(), Error> {
    let pubkeys = collect_pubkeys(ranks, rank_map)?;
    let agg_pubkey = aggregate_pubkeys(&pubkeys)?;
    Ok(agg_pubkey.verify_signature(signature, payload)?)
}

/// Verifies a signature for a split-vote scenario involving two distinct payloads.
///
/// This function handles the "Base3" case where participating validators are split
/// between signing a primary `payload` and a `fallback_payload`. This path is used
/// only in rare edge cases where a fallback vote is required.
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
fn verify_base3(
    payload: &[u8],
    fallback_payload: &[u8],
    signature: &BlsSignature,
    ranks: &[u8],
    max_validators: usize,
    mut rank_map: impl FnMut(usize) -> Option<PopVerified<BlsPubkeyAffine>>,
) -> Result<(), Error> {
    let ranks = decode(ranks, max_validators).map_err(Error::Decode)?;
    match ranks {
        Decoded::Base2(ranks) => verify_single_vote_signature(payload, signature, &ranks, rank_map),
        Decoded::Base3(ranks, fallback_ranks) => {
            check_disjoint(&ranks, &fallback_ranks)?;

            // Must run sequentially because `rank_map` captures `total_stake` (FnMut).
            // We pass a mutable reference for the first call so we can reuse the
            // closure for the second.
            let primary_pubkeys = collect_pubkeys(&ranks, &mut rank_map)?;
            let fallback_pubkeys = collect_pubkeys(&fallback_ranks, rank_map)?;

            if primary_pubkeys.is_empty() {
                let agg_pubkey = aggregate_pubkeys(&fallback_pubkeys)?;
                Ok(agg_pubkey.verify_signature(signature, fallback_payload)?)
            } else if fallback_pubkeys.is_empty() {
                let agg_pubkey = aggregate_pubkeys(&primary_pubkeys)?;
                Ok(agg_pubkey.verify_signature(signature, payload)?)
            } else {
                let agg_primary = aggregate_pubkeys(&primary_pubkeys)?;
                let agg_fallback = aggregate_pubkeys(&fallback_pubkeys)?;

                // SAFETY: `aggregate_pubkeys` strictly requires `PopVerified` public keys
                // as input. Because every constituent key has already proven possession,
                // the resulting aggregated key inherits this property and is mathematically
                // protected against rogue-key attacks, making it safe to bypass the
                // cryptographic check here.
                let all_pubkeys = [
                    unsafe { PopVerified::new_unchecked(*agg_primary) },
                    unsafe { PopVerified::new_unchecked(*agg_fallback) },
                ];

                let all_messages = [payload, fallback_payload];

                if rayon::current_num_threads() < THREAD_POOL_THRESHOLD {
                    Ok(SignatureProjective::verify_distinct_aggregated(
                        all_pubkeys.iter(),
                        signature,
                        all_messages.into_iter(),
                    )?)
                } else {
                    Ok(SignatureProjective::par_verify_distinct_aggregated(
                        &all_pubkeys,
                        signature,
                        &all_messages,
                    )?)
                }
            }
        }
    }
}

/// Aggregates a slice of public keys into a single projective public key.
pub fn aggregate_pubkeys(
    pubkeys: &[PopVerified<BlsPubkeyAffine>],
) -> Result<AggregatePubkey<PubkeyProjective>, Error> {
    if rayon::current_num_threads() < THREAD_POOL_THRESHOLD {
        PubkeyProjective::aggregate(pubkeys.iter()).map_err(Error::VerifySig)
    } else {
        PubkeyProjective::par_aggregate(pubkeys.par_iter()).map_err(Error::VerifySig)
    }
}

/// Collects public keys sequentially based on the provided ranks bitmap.
pub fn collect_pubkeys(
    ranks: &BitVec<u8>,
    mut rank_map: impl FnMut(usize) -> Option<PopVerified<BlsPubkeyAffine>>,
) -> Result<Vec<PopVerified<BlsPubkeyAffine>>, Error> {
    let mut pubkeys = Vec::with_capacity(ranks.count_ones());
    for rank in ranks.iter_ones() {
        let pubkey = rank_map(rank).ok_or(Error::MissingRank)?;
        pubkeys.push(pubkey);
    }
    Ok(pubkeys)
}

/// Ensures that no validator appears in both the primary and fallback bitmaps.
fn check_disjoint(ranks: &BitVec<u8>, fallback_ranks: &BitVec<u8>) -> Result<(), Error> {
    // Ensure both bitmaps are exactly the same length.
    if ranks.len() != fallback_ranks.len() {
        return Err(Error::WrongEncoding);
    }

    // Ensure no validator appears in both bitmaps.
    // We use `iter_ones` for an allocation-free O(popcount) check.
    if ranks.iter_ones().any(|i| fallback_ranks[i]) {
        return Err(Error::BitmapOverlap);
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use {
        super::*,
        agave_votor::consensus_pool::certificate_builder::CertificateBuilder,
        agave_votor_messages::{
            certificate::CertificateType,
            consensus_message::{Block, VoteMessage},
            vote::Vote,
            wire::get_vote_payload_to_sign,
        },
        rand::Rng,
        solana_bls_signatures::{
            keypair::Keypair as BLSKeypair, signature::Signature as BLSSignature,
        },
        solana_hash::Hash,
        solana_signer_store::{encode_base2, encode_base3},
    };

    fn create_bls_keypairs(num_signers: usize) -> Vec<BLSKeypair> {
        (0..num_signers)
            .map(|_| BLSKeypair::new())
            .collect::<Vec<_>>()
    }

    fn create_signed_vote_message(
        bls_keypairs: &[BLSKeypair],
        shred_version: u16,
        vote: Vote,
        rank: usize,
    ) -> VoteMessage {
        let bls_keypair = &bls_keypairs[rank];
        let payload = get_vote_payload_to_sign(vote, shred_version);
        let signature: BLSSignature = bls_keypair.sign(&payload).into();
        VoteMessage {
            vote,
            signature,
            rank: rank as u16,
            stake: NonZero::new(123).unwrap(),
        }
    }

    fn create_signed_certificate_message(
        bls_keypairs: &[BLSKeypair],
        shred_version: u16,
        cert_type: CertificateType,
        ranks: &[usize],
    ) -> UnverifiedCertificate {
        let mut builder = CertificateBuilder::new(cert_type);
        // Assumes Base2 encoding (single vote type) for simplicity in this helper.
        let vote = cert_type.to_source_vote();
        let vote_messages: Vec<VoteMessage> = ranks
            .iter()
            .map(|&rank| create_signed_vote_message(bls_keypairs, shred_version, vote, rank))
            .collect();

        builder
            .aggregate(&vote_messages)
            .expect("Failed to aggregate votes");
        let cert = builder.build().expect("Failed to build certificate");
        UnverifiedCertificate {
            cert_type: cert.cert_type,
            signature: cert.signature,
            bitmap: cert.bitmap,
            shred_version,
        }
    }

    #[test]
    fn test_verify_certificate_base2_valid() {
        let bls_keypairs = create_bls_keypairs(10);
        let shred_version = rand::rng().random();
        let cert_type = CertificateType::Notarize(Block {
            slot: 10,
            block_id: Hash::new_unique(),
        });
        let cert = create_signed_certificate_message(
            &bls_keypairs,
            shred_version,
            cert_type,
            &(0..6).collect::<Vec<_>>(),
        );
        verify_certificate(cert, 10, NonZero::new(600).unwrap(), |rank| {
            bls_keypairs
                .get(rank)
                .map(|kp| (NonZero::new(100).unwrap(), kp.public))
        })
        .unwrap();
    }

    #[test]
    fn test_stake_verification() {
        let bls_keypairs = create_bls_keypairs(10);
        let shred_version = rand::rng().random();
        let cert_type = CertificateType::Notarize(Block {
            slot: 10,
            block_id: Hash::new_unique(),
        });
        let per_validator_stake = 100;
        let num_validators = 10;
        let total_stake = NonZero::new(per_validator_stake * num_validators as u64).unwrap();

        // with enough stake should succeed.
        let cert = create_signed_certificate_message(
            &bls_keypairs,
            shred_version,
            cert_type,
            &(0..6).collect::<Vec<_>>(),
        );
        verify_certificate(cert, 10, total_stake, |rank| {
            bls_keypairs
                .get(rank)
                .map(|kp| (NonZero::new(100).unwrap(), kp.public))
        })
        .unwrap();

        // not enough stake, should fail.
        let cert = create_signed_certificate_message(
            &bls_keypairs,
            shred_version,
            cert_type,
            &(0..5).collect::<Vec<_>>(),
        );
        let Err(err) = verify_certificate(cert, 10, total_stake, |rank| {
            bls_keypairs
                .get(rank)
                .map(|kp| (NonZero::new(100).unwrap(), kp.public))
        }) else {
            panic!("should fail");
        };
        match err {
            Error::NotEnoughStake {
                aggregate_stake,
                cert_fraction,
                required_fraction,
            } => {
                assert_eq!(aggregate_stake, 500);
                assert_eq!(required_fraction, cert_type.threshold());
                assert_eq!(cert_fraction, Fraction::new(500, total_stake));
            }
            rest => panic!("wrong variant {rest:?}"),
        }
    }
    #[test]
    fn test_verify_certificate_base3_valid() {
        let bls_keypairs = create_bls_keypairs(10);
        let shred_version = rand::rng().random();
        let block = Block {
            slot: 20,
            block_id: Hash::new_unique(),
        };
        let notarize_vote = Vote::new_notarization_vote(block);
        let notarize_fallback_vote = Vote::new_notarization_fallback_vote(block);
        let mut all_vote_messages = Vec::new();
        (0..4).for_each(|i| {
            all_vote_messages.push(create_signed_vote_message(
                &bls_keypairs,
                shred_version,
                notarize_vote,
                i,
            ))
        });
        (4..7).for_each(|i| {
            all_vote_messages.push(create_signed_vote_message(
                &bls_keypairs,
                shred_version,
                notarize_fallback_vote,
                i,
            ))
        });
        let cert_type = CertificateType::NotarizeFallback(block);
        let mut builder = CertificateBuilder::new(cert_type);
        builder
            .aggregate(&all_vote_messages)
            .expect("Failed to aggregate votes");
        let cert = builder.build().expect("Failed to build certificate");
        let cert = UnverifiedCertificate {
            cert_type: cert.cert_type,
            signature: cert.signature,
            bitmap: cert.bitmap,
            shred_version,
        };

        verify_certificate(cert, 10, NonZero::new(700).unwrap(), |rank| {
            bls_keypairs
                .get(rank)
                .map(|kp| (NonZero::new(100).unwrap(), kp.public))
        })
        .unwrap();
    }

    #[test]
    fn test_verify_certificate_invalid_signature() {
        let bls_keypairs = create_bls_keypairs(10);
        let shred_version = rand::rng().random();

        let num_signers = 7;
        let block = Block {
            slot: 10,
            block_id: Hash::new_unique(),
        };
        let cert_type = CertificateType::Notarize(block);
        let mut bitmap = BitVec::new();
        bitmap.resize(num_signers, false);
        for i in 0..num_signers {
            bitmap.set(i, true);
        }
        let encoded_bitmap = encode_base2(&bitmap).unwrap();

        let cert = UnverifiedCertificate {
            cert_type,
            signature: BLSSignature([0; 192]), // Use a default/wrong signature
            bitmap: encoded_bitmap,
            shred_version,
        };
        assert_eq!(
            verify_certificate(cert, 10, NonZero::new(1000).unwrap(), |rank| {
                bls_keypairs
                    .get(rank)
                    .map(|kp| (NonZero::new(100).unwrap(), kp.public))
            })
            .unwrap_err(),
            Error::VerifySig(BlsError::PointConversion)
        );
    }

    #[test]
    fn base3_cert_with_no_primary_verifies() {
        let max_validators = 10;
        let bls_keypairs = create_bls_keypairs(max_validators);
        let shred_version = rand::rng().random();
        let block = Block {
            slot: 20,
            block_id: Hash::new_unique(),
        };
        let cert_type = CertificateType::NotarizeFallback(block);
        let mut builder = CertificateBuilder::new(cert_type);
        let vote = Vote::new_notarization_fallback_vote(block);
        let vote_msgs = (0..max_validators)
            .map(|i| create_signed_vote_message(&bls_keypairs, shred_version, vote, i))
            .collect::<Vec<_>>();
        builder.aggregate(&vote_msgs).unwrap();
        let cert = builder.build().unwrap();
        let cert = UnverifiedCertificate {
            cert_type: cert.cert_type,
            signature: cert.signature,
            bitmap: cert.bitmap,
            shred_version,
        };
        let per_validator_stake = 100;
        verify_certificate(
            cert,
            10,
            NonZero::new(per_validator_stake * max_validators as u64).unwrap(),
            |rank| {
                bls_keypairs
                    .get(rank)
                    .map(|kp| (NonZero::new(per_validator_stake).unwrap(), kp.public))
            },
        )
        .unwrap();
    }

    #[test]
    fn test_check_disjoint() {
        let mut ranks = BitVec::<u8>::new();
        let mut fallback_ranks = BitVec::<u8>::new();
        ranks.resize(10, false);
        fallback_ranks.resize(10, false);

        // Honest disjoint bitmaps
        ranks.set(0, true);
        fallback_ranks.set(1, true);
        assert_eq!(check_disjoint(&ranks, &fallback_ranks), Ok(()));

        // Malicious overlapping bitmaps
        fallback_ranks.set(0, true);
        assert_eq!(
            check_disjoint(&ranks, &fallback_ranks),
            Err(Error::BitmapOverlap)
        );

        // Mismatched lengths
        let mut short_ranks = BitVec::<u8>::new();
        short_ranks.resize(5, false);
        assert_eq!(
            check_disjoint(&short_ranks, &fallback_ranks),
            Err(Error::WrongEncoding)
        );
    }

    // ----------------------------------------------------------------------------
    // Adversarial / corruption robustness.
    //
    // These tests cover the unit-level core of "a single bad validator corrupting a
    // BLS certificate" (anza-xyz/alpenglow#474): a tampered certificate must be
    // rejected and must never report stake it cannot prove a signature for. The
    // network-level blacklisting test described in that issue builds on top of this
    // verification contract.
    // ----------------------------------------------------------------------------

    const STAKE_PER_VALIDATOR: NonZero<u64> = NonZero::new(100).unwrap();

    /// A `Block` for `slot` with a fresh, unique block id.
    fn fresh_block(slot: u64) -> Block {
        Block {
            slot,
            block_id: Hash::new_unique(),
        }
    }

    /// Encode a Base2 rank bitmap with the given `ranks` set out of `num_bits`.
    fn encoded_base2_bitmap(ranks: &[usize], num_bits: usize) -> Vec<u8> {
        let mut bitmap = BitVec::new();
        bitmap.resize(num_bits, false);
        for &rank in ranks {
            bitmap.set(rank, true);
        }
        encode_base2(&bitmap).unwrap()
    }

    /// Encode a Base3 rank bitmap where `primary` ranks signed the primary vote and
    /// `fallback` ranks signed the fallback vote. The two sets must be disjoint.
    fn encoded_base3_bitmap(primary: &[usize], fallback: &[usize], num_bits: usize) -> Vec<u8> {
        let mut base = BitVec::new();
        base.resize(num_bits, false);
        for &rank in primary {
            base.set(rank, true);
        }
        let mut fallback_bits = BitVec::new();
        fallback_bits.resize(num_bits, false);
        for &rank in fallback {
            fallback_bits.set(rank, true);
        }
        encode_base3(&base, &fallback_bits).unwrap()
    }

    /// Uniform `rank_map` over `keypairs`, each with `STAKE_PER_VALIDATOR` stake.
    fn rank_map(
        keypairs: &[BLSKeypair],
    ) -> impl FnMut(usize) -> Option<(NonZero<u64>, PopVerified<BlsPubkeyAffine>)> + '_ {
        move |rank| {
            keypairs
                .get(rank)
                .map(|kp| (STAKE_PER_VALIDATOR, kp.public))
        }
    }

    /// A certificate with the given bitmap and a placeholder (all-zero) signature,
    /// for tests whose rejection is independent of the signature contents.
    fn unsigned_cert(
        cert_type: CertificateType,
        bitmap: Vec<u8>,
        shred_version: u16,
    ) -> UnverifiedCertificate {
        UnverifiedCertificate {
            cert_type,
            signature: BLSSignature([0; 192]),
            bitmap,
            shred_version,
        }
    }

    /// Build a valid Base3 `NotarizeFallback` certificate where `primary_ranks` signed
    /// the notarization vote and `fallback_ranks` signed the notarization fallback vote.
    fn signed_notarize_fallback_cert(
        keypairs: &[BLSKeypair],
        shred_version: u16,
        block: Block,
        primary_ranks: &[usize],
        fallback_ranks: &[usize],
    ) -> Certificate {
        let notarize = Vote::new_notarization_vote(block);
        let fallback = Vote::new_notarization_fallback_vote(block);
        let mut messages = Vec::new();
        for &rank in primary_ranks {
            messages.push(create_signed_vote_message(
                keypairs,
                shred_version,
                notarize,
                rank,
            ));
        }
        for &rank in fallback_ranks {
            messages.push(create_signed_vote_message(
                keypairs,
                shred_version,
                fallback,
                rank,
            ));
        }
        let mut builder = CertificateBuilder::new(CertificateType::NotarizeFallback(block));
        builder.aggregate(&messages).unwrap();
        builder.build().unwrap()
    }

    /// Adding a validator to the bitmap who did not actually sign must fail
    /// verification (the aggregate no longer matches the signature) - a bad actor
    /// cannot inflate the signing stake of a certificate.
    #[test]
    fn tampered_bitmap_adding_unsigned_validator_fails() {
        let keypairs = create_bls_keypairs(10);
        let shred_version = rand::rng().random();
        let cert_type = CertificateType::Notarize(fresh_block(10));
        let mut cert = create_signed_certificate_message(
            &keypairs,
            shred_version,
            cert_type,
            &[0, 1, 2, 3, 4, 5],
        );

        // Claim rank 6 also signed, even though it never did.
        cert.bitmap = encoded_base2_bitmap(&[0, 1, 2, 3, 4, 5, 6], 7);

        // Must fail at the signature stage (not via a decode / missing-rank path),
        // proving the extra validator cannot be smuggled in.
        assert_eq!(
            verify_certificate(cert, 10, NonZero::new(10).unwrap(), rank_map(&keypairs))
                .unwrap_err(),
            Error::VerifySig(BlsError::VerificationFailed)
        );
    }

    /// Removing a genuine signer from the bitmap must also fail: the aggregate
    /// signature still carries that signer's contribution, so the reduced key set
    /// will not verify.
    #[test]
    fn tampered_bitmap_removing_signer_fails() {
        let keypairs = create_bls_keypairs(10);
        let shred_version = rand::rng().random();
        let cert_type = CertificateType::Notarize(fresh_block(10));
        let mut cert = create_signed_certificate_message(
            &keypairs,
            shred_version,
            cert_type,
            &[0, 1, 2, 3, 4, 5],
        );

        // Drop rank 5 from the bitmap while keeping its signature in the aggregate.
        cert.bitmap = encoded_base2_bitmap(&[0, 1, 2, 3, 4], 6);

        assert_eq!(
            verify_certificate(cert, 10, NonZero::new(10).unwrap(), rank_map(&keypairs))
                .unwrap_err(),
            Error::VerifySig(BlsError::VerificationFailed)
        );
    }

    /// A certificate whose `cert_type` (here the block id) is altered after the votes
    /// were signed must fail: the reconstructed payload no longer matches what the
    /// validators signed.
    #[test]
    fn tampered_cert_type_payload_fails() {
        let keypairs = create_bls_keypairs(10);
        let shred_version = rand::rng().random();
        let cert = create_signed_certificate_message(
            &keypairs,
            shred_version,
            CertificateType::Notarize(fresh_block(10)),
            &[0, 1, 2, 3, 4, 5],
        );

        let forged = UnverifiedCertificate {
            cert_type: CertificateType::Notarize(fresh_block(10)),
            signature: cert.signature,
            bitmap: cert.bitmap.clone(),
            shred_version,
        };

        assert_eq!(
            verify_certificate(forged, 10, NonZero::new(10).unwrap(), rank_map(&keypairs))
                .unwrap_err(),
            Error::VerifySig(BlsError::VerificationFailed)
        );
    }

    /// A certificate with no signers (empty bitmap) must NOT verify. Accepting it
    /// would let an attacker present a "certificate" backed by zero stake.
    #[test]
    fn empty_bitmap_certificate_does_not_verify() {
        let keypairs = create_bls_keypairs(10);
        let shred_version = rand::rng().random();
        let cert = unsigned_cert(
            CertificateType::Notarize(fresh_block(10)),
            encoded_base2_bitmap(&[], 0),
            shred_version,
        );

        // An empty signer set cannot produce a valid aggregate signature.
        assert_eq!(
            verify_certificate(cert, 10, NonZero::new(10).unwrap(), rank_map(&keypairs))
                .unwrap_err(),
            Error::VerifySig(BlsError::EmptyAggregation)
        );
    }

    /// A Base3-encoded bitmap supplied for a certificate type that is verified as
    /// Base2 (e.g. `Notarize`) must be rejected with `WrongEncoding` rather than
    /// silently mis-decoded.
    #[test]
    fn base3_bitmap_rejected_for_base2_certificate() {
        let keypairs = create_bls_keypairs(10);
        let shred_version = rand::rng().random();
        let bitmap = encoded_base3_bitmap(&[0, 1], &[2], 6);
        let cert = unsigned_cert(
            CertificateType::Notarize(fresh_block(10)),
            bitmap,
            shred_version,
        );

        assert_eq!(
            verify_certificate(cert, 10, NonZero::new(10).unwrap(), rank_map(&keypairs))
                .unwrap_err(),
            Error::WrongEncoding
        );
    }

    /// If a rank present in the bitmap is unknown to the `rank_map` (e.g. not in the
    /// epoch's stake table), verification fails with `MissingRank` rather than
    /// silently skipping that validator.
    #[test]
    fn unknown_rank_in_bitmap_fails() {
        let keypairs = create_bls_keypairs(10);
        let shred_version = rand::rng().random();
        let cert_type = CertificateType::Notarize(fresh_block(10));
        let cert = create_signed_certificate_message(
            &keypairs,
            shred_version,
            cert_type,
            &[0, 1, 2, 3, 4, 5],
        );

        // rank_map returns None for rank 3, simulating an unknown/unstaked validator.
        let result = verify_certificate(cert, 10, NonZero::new(10).unwrap(), |rank| {
            if rank == 3 {
                None
            } else {
                keypairs
                    .get(rank)
                    .map(|kp| (STAKE_PER_VALIDATOR, kp.public))
            }
        });

        assert_eq!(result.unwrap_err(), Error::MissingRank);
    }

    /// A bitmap declaring more bits than `max_validators` must be rejected by the
    /// decoder (no panic, no out-of-bounds rank lookups).
    #[test]
    fn oversized_bitmap_is_rejected() {
        let keypairs = create_bls_keypairs(10);
        let shred_version = rand::rng().random();
        // 20 bits declared, but only 10 validators allowed.
        let cert = unsigned_cert(
            CertificateType::Notarize(fresh_block(10)),
            encoded_base2_bitmap(&[0, 1], 20),
            shred_version,
        );

        assert_eq!(
            verify_certificate(cert, 10, NonZero::new(10).unwrap(), rank_map(&keypairs))
                .unwrap_err(),
            Error::Decode(DecodeError::CorruptDataPayload)
        );
    }

    /// Arbitrary attacker-controlled bitmap bytes must never panic the verifier and
    /// must never verify (these certificates carry no valid aggregate signature).
    #[test]
    fn garbage_bitmap_never_panics_and_never_verifies() {
        let keypairs = create_bls_keypairs(10);
        let shred_version = rand::rng().random();
        for seed in 0u64..512 {
            let len = (seed % 40) as usize;
            let bitmap: Vec<u8> = (0..len)
                .map(|i| seed.wrapping_mul(2_654_435_761).wrapping_add(i as u64 * 7) as u8)
                .collect();
            let cert = unsigned_cert(
                CertificateType::Notarize(fresh_block(10)),
                bitmap,
                shred_version,
            );
            // Must return (Ok/Err) without panicking; a zero signature can never
            // produce a valid certificate, so the result must be an error.
            verify_certificate(cert, 10, NonZero::new(10).unwrap(), rank_map(&keypairs))
                .unwrap_err();
        }
    }

    /// In a Base3 (split-vote) certificate, swapping which payload each validator is
    /// claimed to have signed must fail: every validator's signature is bound to a
    /// specific vote payload, so misattributing it breaks the aggregate check.
    #[test]
    fn base3_tampered_swapping_vote_types_fails() {
        let keypairs = create_bls_keypairs(10);
        let shred_version = rand::rng().random();
        let block = fresh_block(20);
        // 0,1 signed the primary (notarize) vote; 2,3 signed the fallback vote.
        let cert = signed_notarize_fallback_cert(&keypairs, shred_version, block, &[0, 1], &[2, 3]);

        // Forge a bitmap claiming the opposite: 2,3 as primary and 0,1 as fallback.
        let forged = UnverifiedCertificate {
            cert_type: cert.cert_type,
            signature: cert.signature,
            bitmap: encoded_base3_bitmap(&[2, 3], &[0, 1], 4),
            shred_version,
        };

        assert_eq!(
            verify_certificate(forged, 10, NonZero::new(10).unwrap(), rank_map(&keypairs))
                .unwrap_err(),
            Error::VerifySig(BlsError::VerificationFailed)
        );
    }

    /// Adding an unsigned validator to the fallback set of a Base3 certificate must
    /// fail verification - the split-vote path is no easier to inflate than Base2.
    #[test]
    fn base3_tampered_adding_unsigned_validator_fails() {
        let keypairs = create_bls_keypairs(10);
        let shred_version = rand::rng().random();
        let block = fresh_block(20);
        let cert = signed_notarize_fallback_cert(&keypairs, shred_version, block, &[0, 1], &[2, 3]);

        // Claim rank 4 also cast a fallback vote, though it never signed.
        let forged = UnverifiedCertificate {
            cert_type: cert.cert_type,
            signature: cert.signature,
            bitmap: encoded_base3_bitmap(&[0, 1], &[2, 3, 4], 5),
            shred_version,
        };

        assert_eq!(
            verify_certificate(forged, 10, NonZero::new(10).unwrap(), rank_map(&keypairs))
                .unwrap_err(),
            Error::VerifySig(BlsError::VerificationFailed)
        );
    }
}
