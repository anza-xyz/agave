//! This module defines VotePool which tracks verified votes received from other
//! validators and when enough stake has been received, produces appropriate
//! certificates.
//!
//! The pool assumes that the bls-sigverifier has performed all conflicting votes checks.

use {
    crate::{
        aggregate_accumulator::{AggregateAccumulator, AggregateAccumulatorError},
        consensus_pool_service::PoolVote,
    },
    agave_votor_messages::{
        certificate::{Certificate, CertificateType},
        vote::Vote,
    },
    solana_clock::Slot,
    std::{
        collections::{BTreeMap, HashMap},
        num::NonZero,
        sync::Arc,
    },
};

pub(super) struct VotePool {
    max_validators: usize,
    accumulators: HashMap<Vote, AggregateAccumulator>,
}

impl VotePool {
    fn new(max_validators: usize, accumulators: HashMap<Vote, AggregateAccumulator>) -> Self {
        Self {
            max_validators,
            accumulators,
        }
    }

    fn try_produce_cert(
        &self,
        total_stake: NonZero<u64>,
        vote: Vote,
        completed_certs: &BTreeMap<CertificateType, Arc<Certificate>>,
        acc: &AggregateAccumulator,
    ) -> Result<Option<Certificate>, AggregateAccumulatorError> {
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
    ) -> Result<(u64, Option<Certificate>), AggregateAccumulatorError> {
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

enum MaybeVotePool {
    None(HashMap<Vote, AggregateAccumulator>),
    Some(VotePool),
}

const NUM_VOTE_POOLS: usize = 128;

/// Stores a set of `VotePool`s in an array to minimise allocations.
pub(super) struct VotePools {
    /// Consensus pool's view of the current root slot.
    root_slot: Slot,
    /// Offset into the array below where the `VotePool` for root_slot would be stored.
    offset: usize,
    /// Array of `VotePool` to minimise allocations.
    pools: [MaybeVotePool; NUM_VOTE_POOLS],
}

impl VotePools {
    /// Creates a new `VotePools`.
    pub(super) fn new(root_slot: Slot) -> Self {
        let pools = std::array::from_fn(|_| MaybeVotePool::None(HashMap::new()));
        Self {
            root_slot,
            offset: 0,
            pools,
        }
    }

    /// Returns a `VotePool` responsible for the given `slot`.
    pub(super) fn get_vote_pool(&mut self, slot: Slot, max_validators: usize) -> &mut VotePool {
        let diff = slot.checked_sub(self.root_slot).unwrap() as usize;
        assert!(diff < NUM_VOTE_POOLS);
        let ind = self.offset.saturating_add(diff).rem_euclid(NUM_VOTE_POOLS);
        let entry = &mut self.pools[ind];
        if let MaybeVotePool::None(accumulators) = entry {
            let pool = VotePool::new(max_validators, std::mem::take(accumulators));
            *entry = MaybeVotePool::Some(pool);
        }
        let MaybeVotePool::Some(pool) = entry else {
            unreachable!("a vote pool was inserted above")
        };
        pool
    }

    /// Prunes the `VotePool`s that are no longer needed.
    pub(super) fn prune(&mut self, root_slot: Slot) {
        let diff = root_slot.checked_sub(self.root_slot).unwrap();
        let num_pools_to_prune = diff.min(NUM_VOTE_POOLS as Slot) as usize;
        for pool_offset in 0..num_pools_to_prune {
            let ind = (self.offset.saturating_add(pool_offset)).rem_euclid(NUM_VOTE_POOLS);
            let entry = &mut self.pools[ind];
            if let MaybeVotePool::Some(pool) = entry {
                pool.accumulators.clear();
                *entry = MaybeVotePool::None(std::mem::take(&mut pool.accumulators));
            }
        }
        self.root_slot = root_slot;
        self.offset = (self
            .offset
            .saturating_add(diff.rem_euclid(NUM_VOTE_POOLS as Slot) as usize))
        .rem_euclid(NUM_VOTE_POOLS);
    }
}
