//! This module defines VotePool which tracks verified votes received from other
//! validators and when enough stake has been received, produces appropriate
//! certificates.
//!
//! The pool assumes that the bls-sigverifier has performed all conflicting votes checks.

use {
    crate::consensus_pool_service::PoolVote,
    agave_votor_messages::{
        aggregate_accumulator::{AggregateAccumulator, AggregateAccumulatorError},
        certificate::{Certificate, CertificateType},
        vote::Vote,
    },
    std::{
        collections::{BTreeMap, HashMap},
        num::NonZero,
        sync::Arc,
    },
    thiserror::Error,
};

#[derive(Debug, PartialEq, Eq, Error)]
pub(crate) enum VotePoolError {
    #[error("AggregateAccumulator failed with {0}")]
    Accumulating(#[from] AggregateAccumulatorError),
}

pub(super) struct VotePool {
    max_validators: usize,
    accumulators: HashMap<Vote, AggregateAccumulator>,
}

impl VotePool {
    pub(super) fn new(max_validators: usize) -> Self {
        Self {
            max_validators,
            accumulators: HashMap::new(),
        }
    }

    fn try_produce_cert(
        &self,
        total_stake: NonZero<u64>,
        vote: Vote,
        completed_certs: &BTreeMap<CertificateType, Arc<Certificate>>,
        acc: &AggregateAccumulator,
    ) -> Result<Option<Certificate>, VotePoolError> {
        match vote {
            Vote::Notarize(notar) => {
                for cert_type in [
                    CertificateType::FinalizeFast(notar.block),
                    CertificateType::Notarize(notar.block),
                ] {
                    if completed_certs.contains_key(&cert_type) {
                        return Ok(None);
                    }
                    if let Some(c) = acc.try_build_base2_cert(cert_type, total_stake)? {
                        return Ok(Some(c));
                    }
                }
                let nf_cert_type = CertificateType::NotarizeFallback(notar.block);
                if completed_certs.contains_key(&nf_cert_type) {
                    return Ok(None);
                }
                let nf_vote = Vote::new_notarization_fallback_vote(notar.block);
                let Some(fallback_acc) = self.accumulators.get(&nf_vote) else {
                    return Ok(None);
                };
                Ok(AggregateAccumulator::try_build_base3_cert(
                    nf_cert_type,
                    total_stake,
                    Some(acc),
                    fallback_acc,
                )?)
            }

            Vote::NotarizeFallback(nf) => {
                let nf_cert_type = CertificateType::NotarizeFallback(nf.block);
                for cert_type in [
                    CertificateType::FinalizeFast(nf.block),
                    CertificateType::Notarize(nf.block),
                    nf_cert_type,
                ] {
                    if completed_certs.contains_key(&cert_type) {
                        return Ok(None);
                    }
                }
                let notar_vote = Vote::new_notarization_vote(nf.block);
                let primary_acc = self.accumulators.get(&notar_vote);
                Ok(AggregateAccumulator::try_build_base3_cert(
                    nf_cert_type,
                    total_stake,
                    primary_acc,
                    acc,
                )?)
            }

            Vote::Finalize(_) => {
                let cert_type = CertificateType::Finalize(vote.slot());
                if completed_certs.contains_key(&cert_type) {
                    return Ok(None);
                }
                Ok(acc.try_build_base2_cert(cert_type, total_stake)?)
            }

            Vote::Skip(_) => {
                let cert_type = CertificateType::Skip(vote.slot());
                if completed_certs.contains_key(&cert_type) {
                    return Ok(None);
                }
                let sf_vote = Vote::new_skip_fallback_vote(vote.slot());
                match self.accumulators.get(&sf_vote) {
                    None => Ok(acc.try_build_base2_cert(cert_type, total_stake)?),
                    Some(fallback) => Ok(AggregateAccumulator::try_build_base3_cert(
                        cert_type,
                        total_stake,
                        Some(acc),
                        fallback,
                    )?),
                }
            }

            Vote::SkipFallback(_) => {
                let cert_type = CertificateType::Skip(vote.slot());
                if completed_certs.contains_key(&cert_type) {
                    return Ok(None);
                }
                let skip_vote = Vote::new_skip_vote(vote.slot());
                let primary = self.accumulators.get(&skip_vote);
                Ok(AggregateAccumulator::try_build_base3_cert(
                    cert_type,
                    total_stake,
                    primary,
                    acc,
                )?)
            }
            Vote::Genesis(genesis) => {
                let cert_type = CertificateType::Genesis(genesis.block);
                if completed_certs.contains_key(&cert_type) {
                    return Ok(None);
                }
                Ok(acc.try_build_base2_cert(cert_type, total_stake)?)
            }
        }
    }

    /// Adds votes and if some certs can be produced and they are not already included in the completed certs, produces them.
    pub(super) fn add_pool_vote(
        &mut self,
        total_stake: NonZero<u64>,
        msg: &PoolVote,
        completed_certs: &BTreeMap<CertificateType, Arc<Certificate>>,
    ) -> Result<(u64, Option<Certificate>), VotePoolError> {
        let vote = *msg.vote();
        let acc = self
            .accumulators
            .entry(vote)
            .or_insert_with(|| AggregateAccumulator::new(self.max_validators));
        let stake = match msg {
            PoolVote::Own(vote_msg) => acc.add_own_vote_message(vote_msg),
            PoolVote::External(a) => acc.add_aggregate(a),
        }?;
        let acc = self
            .accumulators
            .get(&vote)
            .expect("the accumulator was created above");
        let cert = self.try_produce_cert(total_stake, vote, completed_certs, acc)?;
        Ok((stake, cert))
    }
}
