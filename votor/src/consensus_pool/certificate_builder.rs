use {
    crate::common::certificate_limits_and_votes,
    agave_votor_messages::consensus_message::{Certificate, CertificateType, VoteMessage},
    bitvec::prelude::*,
    solana_bls_signatures::{BlsError, SignatureProjective},
    solana_signer_store::{encode_base2, encode_base3, EncodeError},
    thiserror::Error,
};

/// Maximum number of validators in a certificate
///
/// There are around 1500 validators currently. For a clean power-of-two
/// implementation, we should choose either 2048 or 4096. Choose a more
/// conservative number 4096 for now. During build() we will cut off end
/// of the bitmaps if the tail contains only zeroes, so actual bitmap
/// length will be less than or equal to this number.
const MAXIMUM_VALIDATORS: usize = 4096;

/// Different types of errors that can be returned from the [`CertificateBuilder::aggregate()`] function.
#[derive(Debug, PartialEq, Eq, Error)]
pub(super) enum AggregateError {
    #[error("BLS error: {0}")]
    Bls(#[from] BlsError),
    #[error("Invalid rank: {0}")]
    InvalidRank(u16),
    #[error("Validator already included")]
    ValidatorAlreadyIncluded,
    #[error("assumption for vote_types array broken")]
    InvalidVoteTypes,
}

/// Different types of errors that can be returned from the [`CertificateBuilder::build()`] function.
#[derive(Debug, PartialEq, Eq, Error)]
pub(crate) enum BuildError {
    #[error("BLS error: {0}")]
    Bls(#[from] BlsError),
    #[error("Encoding failed: {0:?}")]
    Encode(EncodeError),
    #[error("Invalid rank: {0}")]
    InvalidRank(usize),
}

fn default_bitvec() -> BitVec<u8, Lsb0> {
    BitVec::repeat(false, MAXIMUM_VALIDATORS)
}

/// Build a [`Certificate`] from a single bitmap.
fn build_cert_from_bitmap(
    cert_type: CertificateType,
    signature: SignatureProjective,
    mut bitmap: BitVec<u8, Lsb0>,
) -> Result<Certificate, BuildError> {
    let new_len = bitmap.last_one().map_or(0, |i| i.saturating_add(1));
    // checks in `aggregate()` guarantee that this assertion is valid
    debug_assert!(new_len <= MAXIMUM_VALIDATORS);
    if new_len > MAXIMUM_VALIDATORS {
        return Err(BuildError::InvalidRank(new_len));
    }
    bitmap.resize(new_len, false);
    let bitmap = encode_base2(&bitmap).map_err(BuildError::Encode)?;
    Ok(Certificate {
        cert_type,
        signature: signature.into(),
        bitmap,
    })
}

/// Build a [`Certificate`] from two bitmaps.
fn build_cert_from_bitmaps(
    cert_type: CertificateType,
    signature: SignatureProjective,
    mut bitmap0: BitVec<u8, Lsb0>,
    mut bitmap1: BitVec<u8, Lsb0>,
) -> Result<Certificate, BuildError> {
    let last_one_0 = bitmap0.last_one().map_or(0, |i| i.saturating_add(1));
    let last_one_1 = bitmap1.last_one().map_or(0, |i| i.saturating_add(1));
    let new_len = last_one_0.max(last_one_1);
    // checks in `aggregate()` guarantee that this assertion is valid
    debug_assert!(new_len <= MAXIMUM_VALIDATORS);
    if new_len > MAXIMUM_VALIDATORS {
        return Err(BuildError::InvalidRank(new_len));
    }
    bitmap0.resize(new_len, false);
    bitmap1.resize(new_len, false);
    let bitmap = encode_base3(&bitmap0, &bitmap1).map_err(BuildError::Encode)?;
    Ok(Certificate {
        cert_type,
        signature: signature.into(),
        bitmap,
    })
}

/// Looks up the bit at `rank` in `bitmap` and sets it to true.
fn try_set_bitmap(bitmap: &mut BitVec<u8, Lsb0>, rank: u16) -> Result<(), AggregateError> {
    let mut ptr = bitmap
        .get_mut(rank as usize)
        .ok_or(AggregateError::InvalidRank(rank))?;
    if *ptr {
        return Err(AggregateError::ValidatorAlreadyIncluded);
    }
    *ptr = true;
    Ok(())
}

/// Internal builder for creating [`Certificate`] by using BLS signature aggregation.
#[allow(clippy::large_enum_variant)]
enum BuilderType {
    /// The produced [`Certificate`] will require only one type of [`VoteMessage`].
    SingleVote {
        signature: SignatureProjective,
        bitmap: BitVec<u8, Lsb0>,
    },
    /// A [`Certificate`] of type NotarFallback or Skip will be produced.
    ///
    /// It can require two types of [`VoteMessage`]s.
    DoubleVote {
        signature: SignatureProjective,
        bitmap0: BitVec<u8, Lsb0>,
        bitmap1: Option<BitVec<u8, Lsb0>>,
    },
}

impl BuilderType {
    /// Creates a new instance of [`BuilderType`].
    fn new(cert_type: &CertificateType) -> Self {
        match cert_type {
            CertificateType::NotarizeFallback(_, _) | CertificateType::Skip(_) => {
                Self::DoubleVote {
                    signature: SignatureProjective::identity(),
                    bitmap0: default_bitvec(),
                    bitmap1: None,
                }
            }
            CertificateType::Finalize(_)
            | CertificateType::FinalizeFast(_, _)
            | CertificateType::Notarize(_, _)
            | CertificateType::Genesis(_, _) => Self::SingleVote {
                signature: SignatureProjective::identity(),
                bitmap: default_bitvec(),
            },
        }
    }

    /// Aggregates new [`VoteMessage`]s into the builder.
    fn aggregate(
        &mut self,
        cert_type: &CertificateType,
        msgs: &[VoteMessage],
    ) -> Result<(), AggregateError> {
        let (_, vote, fallback_vote) = certificate_limits_and_votes(cert_type);
        match self {
            Self::DoubleVote {
                signature,
                bitmap0,
                bitmap1,
            } => {
                debug_assert!(fallback_vote.is_some());
                let Some(fallback_vote) = fallback_vote else {
                    return Err(AggregateError::InvalidVoteTypes);
                };
                for msg in msgs {
                    if msg.vote == vote {
                        try_set_bitmap(bitmap0, msg.rank)?;
                    } else {
                        debug_assert_eq!(msg.vote, fallback_vote);
                        if msg.vote != fallback_vote {
                            return Err(AggregateError::InvalidVoteTypes);
                        }
                        match bitmap1 {
                            Some(bitmap) => try_set_bitmap(bitmap, msg.rank)?,
                            None => {
                                let mut bitmap = default_bitvec();
                                try_set_bitmap(&mut bitmap, msg.rank)?;
                                *bitmap1 = Some(bitmap);
                            }
                        }
                    }
                }
                Ok(signature.aggregate_with(msgs.iter().map(|m| &m.signature))?)
            }

            Self::SingleVote { signature, bitmap } => {
                debug_assert!(fallback_vote.is_none());
                if fallback_vote.is_some() {
                    return Err(AggregateError::InvalidVoteTypes);
                }
                for msg in msgs {
                    debug_assert_eq!(msg.vote, vote);
                    if msg.vote != vote {
                        return Err(AggregateError::InvalidVoteTypes);
                    }
                    try_set_bitmap(bitmap, msg.rank)?;
                }
                Ok(signature.aggregate_with(msgs.iter().map(|m| &m.signature))?)
            }
        }
    }

    /// Builds a [`Certificate`] from the builder.
    fn build(self, cert_type: CertificateType) -> Result<Certificate, BuildError> {
        match self {
            Self::SingleVote { signature, bitmap } => {
                build_cert_from_bitmap(cert_type, signature, bitmap)
            }
            Self::DoubleVote {
                signature,
                bitmap0,
                bitmap1,
            } => match bitmap1 {
                None => build_cert_from_bitmap(cert_type, signature, bitmap0),
                Some(bitmap1) => build_cert_from_bitmaps(cert_type, signature, bitmap0, bitmap1),
            },
        }
    }
}

/// Builder for creating [`Certificate`] by using BLS signature aggregation.
pub(super) struct CertificateBuilder {
    builder_type: BuilderType,
    cert_type: CertificateType,
}

impl CertificateBuilder {
    /// Creates a new instance of the builder.
    pub(super) fn new(cert_type: CertificateType) -> Self {
        let builder_type = BuilderType::new(&cert_type);
        Self {
            builder_type,
            cert_type,
        }
    }

    /// Aggregates new [`VoteMessage`]s into the builder.
    pub(super) fn aggregate(&mut self, msgs: &[VoteMessage]) -> Result<(), AggregateError> {
        self.builder_type.aggregate(&self.cert_type, msgs)
    }

    /// Builds a [`Certificate`] from the builder.
    pub(super) fn build(self) -> Result<Certificate, BuildError> {
        self.builder_type.build(self.cert_type)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        agave_votor_messages::{
            consensus_message::{CertificateType, VoteMessage},
            vote::Vote,
        },
        solana_bls_signatures::{
            Keypair as BLSKeypair, PubkeyProjective as BLSPubkeyProjective,
            Signature as BLSSignature, SignatureProjective, VerifiablePubkey,
        },
        solana_hash::Hash,
        solana_signer_store::{decode, Decoded},
    };

    #[test]
    fn test_normal_build() {
        let hash = Hash::new_unique();
        let cert_type = CertificateType::NotarizeFallback(1, hash);
        let mut builder = CertificateBuilder::new(cert_type);
        // Test building the certificate from Notarize and NotarizeFallback votes
        // Create Notarize on validator 1, 4, 6
        let vote = Vote::new_notarization_vote(1, hash);
        let rank_1 = [1, 4, 6];
        let messages_1 = rank_1
            .iter()
            .map(|&rank| {
                let keypair = BLSKeypair::new();
                let signature = keypair.sign(b"fake_vote_message");
                VoteMessage {
                    vote,
                    signature: signature.into(),
                    rank,
                }
            })
            .collect::<Vec<_>>();
        builder
            .aggregate(&messages_1)
            .expect("Failed to aggregate notarization votes");
        // Create NotarizeFallback on validator 2, 3, 5, 7
        let vote = Vote::new_notarization_fallback_vote(1, hash);
        let rank_2 = [2, 3, 5, 7];
        let messages_2 = rank_2
            .iter()
            .map(|&rank| {
                let keypair = BLSKeypair::new();
                let signature = keypair.sign(b"fake_vote_message_2");
                VoteMessage {
                    vote,
                    signature: signature.into(),
                    rank,
                }
            })
            .collect::<Vec<_>>();
        builder
            .aggregate(&messages_2)
            .expect("Failed to aggregate notarization fallback votes");

        let cert = builder.build().expect("Failed to build certificate");
        assert_eq!(cert.cert_type, cert_type);
        match decode(&cert.bitmap, MAXIMUM_VALIDATORS).expect("Failed to decode bitmap") {
            Decoded::Base3(bitmap1, bitmap2) => {
                assert_eq!(bitmap1.len(), 8);
                assert_eq!(bitmap2.len(), 8);
                for i in rank_1 {
                    assert!(bitmap1[i as usize]);
                }
                assert_eq!(bitmap1.count_ones(), 3);
                for i in rank_2 {
                    assert!(bitmap2[i as usize]);
                }
                assert_eq!(bitmap2.count_ones(), 4);
            }
            _ => panic!("Expected Base3 encoding"),
        }

        // Build a new certificate with only Notarize votes, we should get Base2 encoding
        let mut builder = CertificateBuilder::new(cert_type);
        builder
            .aggregate(&messages_1)
            .expect("Failed to aggregate notarization votes");
        let cert = builder.build().expect("Failed to build certificate");
        assert_eq!(cert.cert_type, cert_type);
        match decode(&cert.bitmap, MAXIMUM_VALIDATORS).expect("Failed to decode bitmap") {
            Decoded::Base2(bitmap1) => {
                assert_eq!(bitmap1.len(), 7);
                for i in rank_1 {
                    assert!(bitmap1[i as usize]);
                }
                assert_eq!(bitmap1.count_ones(), 3);
            }
            _ => panic!("Expected Base2 encoding"),
        }

        // Base2 encoding only applies when the first bitmap is non-empty, if we build another
        // certificate with only NotarizeFallback votes, we should still get Base3 encoding
        let mut builder = CertificateBuilder::new(cert_type);
        builder
            .aggregate(&messages_2)
            .expect("Failed to aggregate notarization fallback votes");
        let cert = builder.build().expect("Failed to build certificate");
        assert_eq!(cert.cert_type, cert_type);
        match decode(&cert.bitmap, MAXIMUM_VALIDATORS).expect("Failed to decode bitmap") {
            Decoded::Base3(bitmap1, bitmap2) => {
                assert_eq!(bitmap1.count_ones(), 0);
                assert_eq!(bitmap2.len(), 8);
                for i in rank_2 {
                    assert!(bitmap2[i as usize]);
                }
                assert_eq!(bitmap2.count_ones(), 4);
            }
            _ => panic!("Expected Base3 encoding"),
        }
    }

    #[test]
    fn test_builder_with_errors() {
        let hash = Hash::new_unique();
        let cert_type = CertificateType::NotarizeFallback(1, hash);
        let mut builder = CertificateBuilder::new(cert_type);

        // Test with a rank that exceeds the maximum allowed
        let vote = Vote::new_notarization_vote(1, hash);
        let vote2 = Vote::new_notarization_fallback_vote(1, hash);
        let rank_out_of_bounds = MAXIMUM_VALIDATORS.saturating_add(1); // Exceeds MAXIMUM_VALIDATORS
        let keypair = BLSKeypair::new();
        let signature = keypair.sign(b"fake_vote_message");
        let message_out_of_bounds = VoteMessage {
            vote,
            signature: signature.into(),
            rank: rank_out_of_bounds as u16,
        };
        assert_eq!(
            builder.aggregate(&[message_out_of_bounds]),
            Err(AggregateError::InvalidRank(rank_out_of_bounds as u16))
        );

        // Test bls error
        let message_with_invalid_signature = VoteMessage {
            vote,
            signature: BLSSignature::default(), // Invalid signature
            rank: 1,
        };
        assert_eq!(
            builder.aggregate(&[message_with_invalid_signature]),
            Err(AggregateError::Bls(BlsError::PointConversion))
        );

        // Test encoding error
        // Create two bitmaps with the same rank set
        let signature = keypair.sign(b"fake_vote_message_2");
        let messages_1 = vec![VoteMessage {
            vote,
            signature: signature.into(),
            rank: 1,
        }];
        let mut builder = CertificateBuilder::new(cert_type);
        builder
            .aggregate(&messages_1)
            .expect("Failed to aggregate notarization votes");
        let messages_2 = vec![VoteMessage {
            vote: vote2,
            signature: signature.into(),
            rank: 1, // Same rank as in messages_1
        }];
        builder
            .aggregate(&messages_2)
            .expect("Failed to aggregate notarization fallback votes");
        assert_eq!(
            builder.build(),
            Err(BuildError::Encode(EncodeError::InvalidBitCombination))
        );
    }

    #[test]
    fn test_certificate_verification_base2_encoding() {
        let slot = 10;
        let hash = Hash::new_unique();
        let cert_type = CertificateType::Notarize(slot, hash);

        // 1. Setup: Create keypairs and a single vote object.
        // All validators will sign the same message, resulting in a single bitmap.
        let num_validators = 5;
        let mut keypairs = Vec::new();
        let mut vote_messages = Vec::new();
        let vote = Vote::new_notarization_vote(slot, hash);
        let serialized_vote = bincode::serialize(&vote).unwrap();

        for i in 0..num_validators {
            let keypair = BLSKeypair::new();
            let signature = keypair.sign(&serialized_vote);
            vote_messages.push(VoteMessage {
                vote,
                signature: signature.into(),
                rank: i as u16,
            });
            keypairs.push(keypair);
        }

        // 2. Generation: Aggregate votes and build the certificate. This will
        // use base2 encoding because it only contains one type of vote.
        let mut builder = CertificateBuilder::new(cert_type);
        builder
            .aggregate(&vote_messages)
            .expect("Failed to aggregate votes");
        let certificate_message = builder.build().expect("Failed to build certificate");

        // 3. Verification: Aggregate the public keys and verify the signature.
        let aggregate_pubkey = BLSPubkeyProjective::aggregate(keypairs.iter().map(|kp| &kp.public))
            .expect("Failed to aggregate public keys");

        let verification_result =
            aggregate_pubkey.verify_signature(&certificate_message.signature, &serialized_vote);

        assert!(
            verification_result.unwrap_or(false),
            "BLS aggregate signature verification failed for base2 encoded certificate"
        );
    }

    #[test]
    fn test_certificate_verification_base3_encoding() {
        let slot = 20;
        let hash = Hash::new_unique();
        // A NotarizeFallback certificate can be composed of both Notarize and NotarizeFallback
        // votes.
        let cert_type = CertificateType::NotarizeFallback(slot, hash);

        // 1. Setup: Create two groups of validators signing two different vote types.
        let mut all_vote_messages = Vec::new();
        let mut all_pubkeys = Vec::new();
        let mut all_messages = Vec::new();

        // Group 1: Signs a Notarize vote.
        let notarize_vote = Vote::new_notarization_vote(slot, hash);
        let serialized_notarize_vote = bincode::serialize(&notarize_vote).unwrap();
        for i in 0..3 {
            let keypair = BLSKeypair::new();
            let signature = keypair.sign(&serialized_notarize_vote);
            all_vote_messages.push(VoteMessage {
                vote: notarize_vote,
                signature: signature.into(),
                rank: i as u16, // Ranks 0, 1, 2
            });
            all_pubkeys.push(keypair.public);
            all_messages.push(serialized_notarize_vote.clone());
        }

        // Group 2: Signs a NotarizeFallback vote.
        let notarize_fallback_vote = Vote::new_notarization_fallback_vote(slot, hash);
        let serialized_fallback_vote = bincode::serialize(&notarize_fallback_vote).unwrap();
        for i in 3..6 {
            let keypair = BLSKeypair::new();
            let signature = keypair.sign(&serialized_fallback_vote);
            all_vote_messages.push(VoteMessage {
                vote: notarize_fallback_vote,
                signature: signature.into(),
                rank: i as u16, // Ranks 3, 4, 5
            });
            all_pubkeys.push(keypair.public);
            all_messages.push(serialized_fallback_vote.clone());
        }

        // 2. Generation: Aggregate votes. Because there are two vote types, this will use
        //    base3 encoding.
        let mut builder = CertificateBuilder::new(cert_type);
        builder
            .aggregate(&all_vote_messages)
            .expect("Failed to aggregate votes");
        let certificate_message = builder.build().expect("Failed to build certificate");

        // 3. Verification:
        let decoded_bitmap =
            decode(&certificate_message.bitmap, MAXIMUM_VALIDATORS).expect("Failed to decode");

        match decoded_bitmap {
            Decoded::Base2(_bitmap) => {
                panic!("Expected Base3 encoding, but got Base2 encoding");
            }
            Decoded::Base3(bitmap1, bitmap2) => {
                // Bitmap1 should correspond to the Notarize votes (ranks 0, 1, 2)
                assert_eq!(bitmap1.count_ones(), 3);
                assert!(bitmap1[0] && bitmap1[1] && bitmap1[2]);
                // Bitmap2 should correspond to the NotarizeFallback votes (ranks 3, 4, 5)
                assert_eq!(bitmap2.count_ones(), 3);
                assert!(bitmap2[3] && bitmap2[4] && bitmap2[5]);
            }
        }

        SignatureProjective::verify_distinct_aggregated(
            all_pubkeys.iter(),
            &certificate_message.signature,
            all_messages.iter().map(Vec::as_slice),
        )
        .unwrap();
    }
}
