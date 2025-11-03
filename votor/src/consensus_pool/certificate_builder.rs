use {
    agave_votor_messages::consensus_message::VoteMessage,
    bitvec::prelude::*,
    solana_bls_signatures::{BlsError, Signature as BLSSignature, SignatureProjective},
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

/// Different types of errors that can be returned from the [`CertificateBuilder::build()`] function.
#[derive(Debug, PartialEq, Eq, Error)]
pub(crate) enum BuildError {
    #[error("BLS error: {0}")]
    Bls(#[from] BlsError),
    #[error("Encoding failed: {0:?}")]
    Encode(EncodeError),
    #[error("Invalid rank: {0}")]
    InvalidRank(u16),
    #[error("validator already included")]
    ValidatorAlreadyIncluded,
    #[error("votes and fallback overlap")]
    OverlappingVotes,
}

fn default_bitvec() -> BitVec<u8, Lsb0> {
    BitVec::repeat(false, MAXIMUM_VALIDATORS)
}

/// Looks up the bit at `rank` in `bitmap` and sets it to true.
fn try_set_bitmap(bitmap: &mut BitVec<u8, Lsb0>, rank: u16) -> Result<(), BuildError> {
    let mut ptr = bitmap
        .get_mut(rank as usize)
        .ok_or(BuildError::InvalidRank(rank))?;
    if *ptr {
        return Err(BuildError::ValidatorAlreadyIncluded);
    }
    *ptr = true;
    Ok(())
}

/// Constructs a [`BLSSignature`] and a base-2 bitmap suitable for constructing a [`Certificate`].
fn build_single(votes: &[VoteMessage]) -> Result<(BLSSignature, Vec<u8>), BuildError> {
    let mut bitmap = default_bitvec();
    for vote in votes {
        try_set_bitmap(&mut bitmap, vote.rank)?;
    }
    let new_len = bitmap.last_one().map_or(0, |i| i.saturating_add(1));
    bitmap.resize(new_len, false);
    let bitmap = encode_base2(&bitmap).map_err(BuildError::Encode)?;
    let mut signature = SignatureProjective::identity();
    signature.aggregate_with(votes.iter().map(|m| &m.signature))?;
    Ok((signature.into(), bitmap))
}

/// Constructs a [`BLSSignature`] and a base-3 bitmap suitable for constructing a [`Certificate`].
fn build_double(
    votes: &[VoteMessage],
    fallback: &[VoteMessage],
) -> Result<(BLSSignature, Vec<u8>), BuildError> {
    let mut bitmap0 = default_bitvec();
    for vote in votes {
        try_set_bitmap(&mut bitmap0, vote.rank)?;
    }
    let mut bitmap1 = default_bitvec();
    for vote in fallback {
        if bitmap0[vote.rank as usize] {
            return Err(BuildError::OverlappingVotes);
        }
        try_set_bitmap(&mut bitmap1, vote.rank)?;
    }
    let last_one_0 = bitmap0.last_one().map_or(0, |i| i.saturating_add(1));
    let last_one_1 = bitmap1.last_one().map_or(0, |i| i.saturating_add(1));
    let new_len = last_one_0.max(last_one_1);
    bitmap0.resize(new_len, false);
    bitmap1.resize(new_len, false);
    let bitmap = encode_base3(&bitmap0, &bitmap1).map_err(BuildError::Encode)?;

    let mut signature = SignatureProjective::identity();
    signature.aggregate_with(votes.iter().chain(fallback.iter()).map(|m| &m.signature))?;
    Ok((signature.into(), bitmap))
}

/// Constructs a [`BLSSignature`] and a bitmap suitable for constructing a [`Certificate`].
///
/// If fallback is None, then the returned bitmap is base-2 encoded else it is base-3 encoded.
pub(super) fn build_sig_and_bitmap(
    votes: &[VoteMessage],
    fallback: Option<&[VoteMessage]>,
) -> Result<(BLSSignature, Vec<u8>), BuildError> {
    match fallback {
        None => build_single(votes),
        Some(fallback) => build_double(votes, fallback),
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        agave_votor_messages::{consensus_message::VoteMessage, vote::Vote},
        solana_bls_signatures::{
            Keypair as BLSKeypair, Signature as BLSSignature, SignatureProjective,
        },
        solana_hash::Hash,
        solana_signer_store::{decode, Decoded},
    };

    #[test]
    fn validate_try_set_bitmap() {
        let mut bitmap = default_bitvec();
        try_set_bitmap(&mut bitmap, 0).unwrap();
        assert!(matches!(
            try_set_bitmap(&mut bitmap, 0).unwrap_err(),
            BuildError::ValidatorAlreadyIncluded
        ));

        let maximum_validators = MAXIMUM_VALIDATORS.try_into().unwrap();
        assert!(matches!(
            try_set_bitmap(&mut bitmap, maximum_validators).unwrap_err(),
            BuildError::InvalidRank(_)
        ));
    }

    #[test]
    fn validate_build_single() {
        let keypair = BLSKeypair::new();
        let signature = keypair.sign(b"fake_vote_message");
        let rank = 123;
        let vote = Vote::new_notarization_vote(1, Hash::new_unique());

        let invalid_sig_vote = VoteMessage {
            vote,
            signature: BLSSignature::default(),
            rank,
        };
        assert!(matches!(
            build_single(&[invalid_sig_vote]).unwrap_err(),
            BuildError::Bls(BlsError::PointConversion)
        ));

        let vote_message = VoteMessage {
            vote,
            signature: signature.into(),
            rank,
        };
        let (signature, bitmap) = build_single(&[vote_message]).unwrap();
        match decode(&bitmap, MAXIMUM_VALIDATORS).unwrap() {
            Decoded::Base2(bitmap) => {
                assert_eq!(bitmap.count_ones(), 1);
                assert!(bitmap[rank as usize]);
            }
            rest => panic!("{rest:?}"),
        }
        SignatureProjective::verify_distinct_aggregated(
            [keypair.public].iter(),
            &signature,
            [bincode::serialize(&vote).unwrap().as_slice()].into_iter(),
        )
        .unwrap();
    }

    #[test]
    fn validate_build_double() {
        let keypair = BLSKeypair::new();
        let signature = keypair.sign(b"fake_vote_message");
        let rank = 123;
        let vote = Vote::new_notarization_vote(1, Hash::new_unique());
        let vote_message = VoteMessage {
            vote,
            signature: signature.into(),
            rank,
        };
        assert!(matches!(
            build_double(
                std::slice::from_ref(&vote_message),
                std::slice::from_ref(&vote_message)
            )
            .unwrap_err(),
            BuildError::OverlappingVotes
        ));

        let fallback_rank = 124;
        let fallback = VoteMessage {
            vote,
            signature: signature.into(),
            rank: fallback_rank,
        };
        let (signature, bitmap) = build_double(&[vote_message], &[fallback]).unwrap();
        match decode(&bitmap, MAXIMUM_VALIDATORS).unwrap() {
            Decoded::Base3(bitmap0, bitmap1) => {
                assert_eq!(bitmap0.count_ones(), 1);
                assert_eq!(bitmap1.count_ones(), 1);
                assert!(bitmap0[rank as usize]);
                assert!(bitmap1[fallback_rank as usize]);
            }
            rest => panic!("{rest:?}"),
        }
        let vote = bincode::serialize(&vote).unwrap();
        SignatureProjective::verify_distinct_aggregated(
            [keypair.public, keypair.public].iter(),
            &signature,
            [vote.clone(), vote].iter().map(Vec::as_slice),
        )
        .unwrap();
    }
}
