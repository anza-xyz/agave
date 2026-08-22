//! Defines `AggregateAccumulator` that can used to aggregate votes and produce certificates.

use {
    agave_votor_messages::{
        certificate::{Certificate, CertificateType},
        consensus_message::VoteMessage,
        fraction::Fraction,
        sig_verified_messages::VoteAggregate,
    },
    bitvec::vec::BitVec,
    solana_bls_signatures::{
        BlsError, Signature as BLSSignature, SignatureCompressed as BLSSignatureCompressed,
        SignatureProjective,
    },
    solana_signer_store::{EncodeError, encode_base2, encode_base3},
    std::num::NonZero,
    thiserror::Error,
};

/// Different types of errors returned when using [`AggregateAccumulator`]
#[derive(Debug, PartialEq, Eq, Error)]
pub enum AggregateAccumulatorError {
    /// Signature aggregation failed.
    #[error("Signature aggregation failed with {0}")]
    SignatureAggregationFailed(BlsError),
    /// Encoding failed.
    #[error("encoding failed with {0:?}")]
    EncodingFailed(EncodeError),
    /// Duplicate aggregate
    #[error("duplicate aggregate")]
    Duplicate,
}

fn default_bitvec(max_validators: usize) -> BitVec<u8> {
    BitVec::repeat(false, max_validators)
}

#[derive(Debug, Clone)]
/// Accumulates [`VoteAggregate`]s and then can build [`Certificate`] from them.
pub struct AggregateAccumulator {
    ranks: BitVec<u8>,
    signature: SignatureProjective,
    stake: u64,
}

impl AggregateAccumulator {
    /// Constructs a new accumulator.
    pub fn new(max_validators: usize) -> Self {
        Self {
            ranks: default_bitvec(max_validators),
            signature: SignatureProjective::identity(),
            stake: 0,
        }
    }

    /// Accumulate a vote aggregate into the accumulator.
    pub fn add_aggregate(
        &mut self,
        aggregate: &VoteAggregate,
    ) -> Result<u64, AggregateAccumulatorError> {
        self.signature
            .aggregate_with(std::iter::once(aggregate.signature()))
            .map_err(AggregateAccumulatorError::SignatureAggregationFailed)?;
        self.ranks |= aggregate.ranks();
        self.stake = self.stake.saturating_add(aggregate.stake().get());
        Ok(self.stake)
    }

    /// Accumulate own vote message into the accumulator.
    ///
    /// Due to nodes restarting or failover, etc. it is possible to get duplicates.
    pub fn add_own_vote_message(
        &mut self,
        msg: &VoteMessage,
    ) -> Result<u64, AggregateAccumulatorError> {
        let mut signature = self.signature;
        signature
            .aggregate_with(std::iter::once(&msg.signature))
            .map_err(AggregateAccumulatorError::SignatureAggregationFailed)?;
        if self.ranks.replace(msg.rank as usize, true) {
            return Err(AggregateAccumulatorError::Duplicate);
        }
        self.signature = signature;
        self.stake = self.stake.saturating_add(msg.stake.get());
        Ok(self.stake)
    }

    /// Builds a base2 [`Certificate`] if its threshold is met.
    pub fn try_build_base2_cert(
        &self,
        cert_type: CertificateType,
        total_stake: NonZero<u64>,
    ) -> Result<Option<Certificate>, AggregateAccumulatorError> {
        let observed_fraction = Fraction::new(self.stake, total_stake);
        if observed_fraction < cert_type.threshold() {
            return Ok(None);
        }
        let mut ranks = self.ranks.clone();
        let new_len = ranks.last_one().map_or(0, |i| i.saturating_add(1));
        ranks.resize(new_len, false);
        let bitmap = encode_base2(&ranks).map_err(AggregateAccumulatorError::EncodingFailed)?;
        let signature = BLSSignature::from(self.signature);
        Ok(Some(Certificate {
            cert_type,
            signature,
            bitmap,
        }))
    }

    /// Builds a [`Certificate`] from primary and fallback accumulators.
    ///
    /// Uses base3 encoding when the fallback partition is usable, and base2 encoding when only
    /// the primary partition is usable.
    pub fn try_build_base3_cert(
        cert_type: CertificateType,
        total_stake: NonZero<u64>,
        primary: Option<&AggregateAccumulator>,
        fallback: &AggregateAccumulator,
    ) -> Result<Option<Certificate>, AggregateAccumulatorError> {
        // Each non-empty base3 partition is verified against its own aggregate public key.
        // Individually valid votes can still be chosen such that their signatures cancel to the
        // identity within one partition, making any certificate containing that partition
        // invalid. Treat an identity partition as absent and do not count its stake.
        let primary = primary.filter(|accumulator| !accumulator.is_identity());
        if fallback.is_identity() {
            return match primary {
                Some(primary) => primary.try_build_base2_cert(cert_type, total_stake),
                None => Ok(None),
            };
        }

        let observed_fraction = Fraction::new(
            fallback
                .stake()
                .saturating_add(primary.map(|p| p.stake).unwrap_or(0)),
            total_stake,
        );
        if observed_fraction < cert_type.threshold() {
            return Ok(None);
        }
        let (primary_ranks, fallback_ranks, signature) = match primary {
            None => {
                let mut fallback_ranks = fallback.ranks.clone();
                let fallback_len = fallback_ranks.last_one().map_or(0, |i| i.saturating_add(1));
                fallback_ranks.resize(fallback_len, false);
                let primary = BitVec::repeat(false, fallback_len);
                (primary, fallback_ranks, fallback.signature)
            }
            Some(primary) => {
                let mut signature = primary.signature;
                let mut primary_ranks = primary.ranks.clone();
                let mut fallback_ranks = fallback.ranks.clone();
                let last_one_0 = primary_ranks.last_one().map_or(0, |i| i.saturating_add(1));
                let last_one_1 = fallback_ranks.last_one().map_or(0, |i| i.saturating_add(1));
                let new_length = last_one_0.max(last_one_1);
                primary_ranks.resize(new_length, false);
                fallback_ranks.resize(new_length, false);
                signature
                    .aggregate_with(std::iter::once(&fallback.signature))
                    .map_err(AggregateAccumulatorError::SignatureAggregationFailed)?;
                (primary_ranks, fallback_ranks, signature)
            }
        };
        let bitmap = encode_base3(&primary_ranks, &fallback_ranks)
            .map_err(AggregateAccumulatorError::EncodingFailed)?;
        Ok(Some(Certificate {
            cert_type,
            signature: signature.into(),
            bitmap,
        }))
    }

    /// Returns the aggregated signature and ranks from the accumulated VoteAggregates so far.
    pub fn into_sig_and_ranks(self) -> Result<(BLSSignatureCompressed, Vec<u8>), EncodeError> {
        let mut ranks = self.ranks;
        let new_len = ranks.last_one().map_or(0, |i| i.saturating_add(1));
        ranks.resize(new_len, false);
        let ranks = encode_base2(&ranks)?;
        let signature = BLSSignature::from(self.signature).try_into().unwrap();
        Ok((signature, ranks))
    }

    /// Accessor for stake
    pub fn stake(&self) -> u64 {
        self.stake
    }

    fn is_identity(&self) -> bool {
        self.signature == SignatureProjective::identity()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_bls_signatures::Keypair as BlsKeypair,
        solana_signer_store::{Decoded, decode},
    };

    const MAX_VALIDATORS: usize = 4;

    fn new_accumulator(
        ranks: &[usize],
        stake: u64,
        signature: SignatureProjective,
    ) -> AggregateAccumulator {
        let mut accumulator = AggregateAccumulator::new(MAX_VALIDATORS);
        for &rank in ranks {
            accumulator.ranks.set(rank, true);
        }
        accumulator.signature = signature;
        accumulator.stake = stake;
        accumulator
    }

    #[test]
    fn test_base3_identity_fallback_uses_primary_base2() {
        let cert_type = CertificateType::Skip(1);
        let primary_signature = BlsKeypair::new().sign(b"primary");
        let primary = new_accumulator(&[0, 1], 60, primary_signature);
        let fallback = new_accumulator(&[2, 3], 2, SignatureProjective::identity());

        let cert = AggregateAccumulator::try_build_base3_cert(
            cert_type,
            NonZero::new(100).unwrap(),
            Some(&primary),
            &fallback,
        )
        .unwrap()
        .unwrap();

        assert_eq!(cert.signature, BLSSignature::from(primary_signature));
        let Decoded::Base2(ranks) = decode(&cert.bitmap, MAX_VALIDATORS).unwrap() else {
            panic!("expected base2 encoding");
        };
        assert_eq!(ranks.count_ones(), 2);
        assert!(ranks[0]);
        assert!(ranks[1]);
    }

    #[test]
    fn test_base3_identity_fallback_stake_is_ignored() {
        let primary = new_accumulator(&[0, 1], 58, BlsKeypair::new().sign(b"primary"));
        let fallback = new_accumulator(&[2, 3], 2, SignatureProjective::identity());

        let cert = AggregateAccumulator::try_build_base3_cert(
            CertificateType::Skip(1),
            NonZero::new(100).unwrap(),
            Some(&primary),
            &fallback,
        )
        .unwrap();

        assert!(cert.is_none());
    }

    #[test]
    fn test_base3_identity_primary_uses_fallback_only() {
        let fallback_signature = BlsKeypair::new().sign(b"fallback");
        let primary = new_accumulator(&[0, 1], 2, SignatureProjective::identity());
        let fallback = new_accumulator(&[2, 3], 60, fallback_signature);

        let cert = AggregateAccumulator::try_build_base3_cert(
            CertificateType::Skip(1),
            NonZero::new(100).unwrap(),
            Some(&primary),
            &fallback,
        )
        .unwrap()
        .unwrap();

        assert_eq!(cert.signature, BLSSignature::from(fallback_signature));
        let Decoded::Base3(primary_ranks, fallback_ranks) =
            decode(&cert.bitmap, MAX_VALIDATORS).unwrap()
        else {
            panic!("expected base3 encoding");
        };
        assert_eq!(primary_ranks.count_ones(), 0);
        assert_eq!(fallback_ranks.count_ones(), 2);
        assert!(fallback_ranks[2]);
        assert!(fallback_ranks[3]);
    }

    #[test]
    fn test_base3_identity_primary_stake_is_ignored() {
        let primary = new_accumulator(&[0, 1], 2, SignatureProjective::identity());
        let fallback = new_accumulator(&[2, 3], 58, BlsKeypair::new().sign(b"fallback"));

        let cert = AggregateAccumulator::try_build_base3_cert(
            CertificateType::Skip(1),
            NonZero::new(100).unwrap(),
            Some(&primary),
            &fallback,
        )
        .unwrap();

        assert!(cert.is_none());
    }
}
