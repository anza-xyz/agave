use {
    agave_votor_messages::{
        certificate::{Certificate, CertificateType},
        consensus_message::VoteMessage,
    },
    bitvec::prelude::*,
    solana_bls_signatures::{BlsError, SignatureProjective},
    solana_signer_store::{EncodeError, encode_base2, encode_base3},
    thiserror::Error,
};

/// Maximum number of validators in a certificate.
///
/// There are around 1500 validators currently. For a clean power-of-two
/// implementation, we should choose either 2048 or 4096. Choose a more
/// conservative number 4096 for now. During build() we will cut off end
/// of the bitmaps if the tail contains only zeroes, so actual bitmap
/// length will be less than or equal to this number.
const MAXIMUM_VALIDATORS: usize = 4096;

/// Different types of errors that can be returned from the [`CertificateBuilder::aggregate()`] function.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum AggregateError {
    #[error("BLS error: {0}")]
    Bls(#[from] BlsError),
    #[error("Invalid rank: {0}")]
    InvalidRank(u16),
    #[error("Validator already included")]
    ValidatorAlreadyIncluded,
}

/// Different types of errors that can be returned from the [`CertificateBuilder::build()`] function.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum BuildError {
    #[error("Encoding failed: {0:?}")]
    Encode(EncodeError),
    #[error("BLS error: {0}")]
    Bls(#[from] BlsError),
}

fn default_bitvec() -> BitVec<u8, Lsb0> {
    BitVec::repeat(false, MAXIMUM_VALIDATORS)
}

/// Build a [`Certificate`] from a single bitmap.
fn build_cert_from_bitmap(
    cert_type: CertificateType,
    signature: SignatureProjective,
    mut bitmap: BitVec<u8, Lsb0>,
) -> Result<Certificate, EncodeError> {
    let new_len = bitmap.last_one().map_or(0, |i| i.saturating_add(1));
    bitmap.resize(new_len, false);
    let bitmap = encode_base2(&bitmap)?;
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
) -> Result<Certificate, EncodeError> {
    let last_one_0 = bitmap0.last_one().map_or(0, |i| i.saturating_add(1));
    let last_one_1 = bitmap1.last_one().map_or(0, |i| i.saturating_add(1));
    let new_length = last_one_0.max(last_one_1);
    bitmap0.resize(new_length, false);
    bitmap1.resize(new_length, false);
    let bitmap = encode_base3(&bitmap0, &bitmap1)?;
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

    /// The produced [`Certificate`] will require two types of [`VoteMessage`]s.
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
            CertificateType::Skip(_) | CertificateType::NotarizeFallback(_) => Self::DoubleVote {
                signature: SignatureProjective::identity(),
                bitmap0: default_bitvec(),
                bitmap1: None,
            },
            _ => Self::SingleVote {
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
        let vote_types = cert_type.limits_and_vote_types().1;
        match self {
            Self::DoubleVote {
                signature,
                bitmap0,
                bitmap1,
            } => {
                assert_eq!(vote_types.len(), 2);
                for msg in msgs {
                    let vote_type = msg.vote.get_type();
                    if vote_type == vote_types[0] {
                        try_set_bitmap(bitmap0, msg.rank)?;
                    } else {
                        assert_eq!(vote_type, vote_types[1]);
                        let bitmap = bitmap1.get_or_insert_with(default_bitvec);
                        try_set_bitmap(bitmap, msg.rank)?;
                    }
                }
                Ok(signature.aggregate_with(msgs.iter().map(|m| &m.signature))?)
            }

            Self::SingleVote { signature, bitmap } => {
                assert_eq!(vote_types.len(), 1);
                for msg in msgs {
                    assert_eq!(msg.vote.get_type(), vote_types[0]);
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
                build_cert_from_bitmap(cert_type, signature, bitmap).map_err(BuildError::Encode)
            }
            Self::DoubleVote {
                signature,
                bitmap0,
                bitmap1,
            } => match bitmap1 {
                None => build_cert_from_bitmap(cert_type, signature, bitmap0)
                    .map_err(BuildError::Encode),
                Some(bitmap1) => build_cert_from_bitmaps(cert_type, signature, bitmap0, bitmap1)
                    .map_err(BuildError::Encode),
            },
        }
    }
}

/// Builder for creating [`Certificate`] by using BLS signature aggregation.
pub struct CertificateBuilder {
    builder_type: BuilderType,
    cert_type: CertificateType,
}

impl CertificateBuilder {
    /// Creates a new instance of the builder.
    pub fn new(cert_type: CertificateType) -> Self {
        let builder_type = BuilderType::new(&cert_type);
        Self {
            builder_type,
            cert_type,
        }
    }

    /// Aggregates new [`VoteMessage`]s into the builder.
    pub fn aggregate(&mut self, msgs: &[VoteMessage]) -> Result<(), AggregateError> {
        self.builder_type.aggregate(&self.cert_type, msgs)
    }

    /// Builds a [`Certificate`] from the builder.
    pub fn build(self) -> Result<Certificate, BuildError> {
        self.builder_type.build(self.cert_type)
    }
}
