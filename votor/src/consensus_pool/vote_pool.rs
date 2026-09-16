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
    thiserror::Error,
};

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

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum VotePoolError {
    #[error("Adding vote to vote_pool failed with {0}")]
    AddVote(AggregateAccumulatorError),
    #[error("old vote: root_slot:{root_slot}, vote_slot:{vote_slot}")]
    OldVote { root_slot: Slot, vote_slot: Slot },
}

pub(super) struct VotePools {
    pools: Vec<Option<VotePool>>,
    root_slot: Slot,
    offset: usize,
}

impl VotePools {
    pub(super) fn new(root_slot: Slot) -> Self {
        const INITIAL_POOLS: usize = 128;
        let pools = (0..INITIAL_POOLS).map(|_| None).collect();
        Self {
            pools,
            root_slot,
            offset: 0,
        }
    }

    pub(super) fn add_pool_vote(
        &mut self,
        max_validators: usize,
        total_stake: NonZero<u64>,
        msg: &PoolVote,
        completed_certs: &BTreeMap<CertificateType, Arc<Certificate>>,
    ) -> Result<(u64, Option<Certificate>), VotePoolError> {
        let vote_slot = msg.vote().slot();
        if vote_slot < self.root_slot {
            return Err(VotePoolError::OldVote {
                root_slot: self.root_slot,
                vote_slot,
            });
        }
        let diff = (vote_slot.saturating_sub(self.root_slot)) as usize;
        if diff >= self.pools.len() {
            // Put the root at index zero before changing the ring length so
            // existing pools retain their root-relative slot positions.
            self.pools.rotate_left(self.offset);
            self.offset = 0;
            // resize the pool either doubling or growing by diff.
            let new_len = (diff.saturating_add(1)).max(self.pools.len().saturating_mul(2));
            self.pools.resize_with(new_len, || None);
        }
        let ind = (self.offset.saturating_add(diff)).rem_euclid(self.pools.len());
        match &mut self.pools[ind] {
            None => {
                let mut pool = VotePool::new(max_validators);
                let res = pool
                    .add_pool_vote(total_stake, msg, completed_certs)
                    .map_err(VotePoolError::AddVote)?;
                self.pools[ind] = Some(pool);
                Ok(res)
            }
            Some(pool) => pool
                .add_pool_vote(total_stake, msg, completed_certs)
                .map_err(VotePoolError::AddVote),
        }
    }

    pub(super) fn purge(&mut self, root_slot: Slot) {
        let Some(diff) = root_slot.checked_sub(self.root_slot) else {
            return;
        };
        let diff = diff as usize;
        if diff >= self.pools.len() {
            self.pools.fill_with(|| None);
            for ind in 0..self.pools.len() {
                self.pools[ind] = None;
            }
            self.offset = 0;
        } else {
            for ind in self.offset..(self.offset.saturating_add(diff)) {
                let ind = ind.rem_euclid(self.pools.len());
                self.pools[ind] = None;
            }
            self.offset = (self.offset.saturating_add(diff)).rem_euclid(self.pools.len());
        }
        self.root_slot = root_slot;
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::consensus_pool::tests::TestContext,
        agave_votor_messages::{
            consensus_message::BLS_KEYPAIR_DERIVE_SEED, wire::get_vote_payload_to_sign,
        },
        solana_bls_signatures::{keypair::Keypair as BLSKeypair, signature::SignatureAffine},
    };

    fn add_vote(
        ctx: &TestContext,
        pools: &mut VotePools,
        slot: Slot,
        rank: usize,
    ) -> Result<(u64, u64), VotePoolError> {
        let bank = ctx.bank_forks.read().unwrap().root_bank();
        let total_stake = bank.get_rank_map(0).unwrap().total_stake();
        // Use the same validator set across distant slots without needing
        // to populate future epoch stakes in the test bank.
        let mut msg = ctx.new_vote_msg(rank, Vote::new_skip_vote(0));
        msg.vote = Vote::new_skip_vote(slot);
        let keypair = BLSKeypair::derive_from_signer(
            &ctx.validators[rank].vote_keypair,
            BLS_KEYPAIR_DERIVE_SEED,
        )
        .unwrap();
        msg.signature = SignatureAffine::from(keypair.sign(&get_vote_payload_to_sign(
            msg.vote,
            ctx.pool.cluster_info.my_shred_version(),
        )));
        let stake = msg.stake.get();
        let (accumulated, _) = pools.add_pool_vote(
            ctx.validators.len(),
            total_stake,
            &PoolVote::Own(msg),
            &BTreeMap::new(),
        )?;
        Ok((accumulated, stake))
    }

    #[test]
    fn test_vote_pools_grow_and_purge() {
        let ctx = TestContext::new();
        let mut pools = VotePools::new(0);
        let add_vote =
            |pools: &mut VotePools, slot, rank| add_vote(&ctx, pools, slot, rank).unwrap();

        // Keep pools on both sides of a wrapped ring, then grow it twice.
        pools.purge(127);
        for slot in [127, 254, 255, 1024] {
            let (accumulated, stake) = add_vote(&mut pools, slot, 0);
            assert_eq!(accumulated, stake);
        }
        for slot in [127, 254, 255, 1024] {
            let (accumulated, stake) = add_vote(&mut pools, slot, 1);
            assert_eq!(accumulated, 2 * stake);
        }

        // Partial purges retain the new root's votes in the expanded ring.
        pools.purge(1024);
        let (accumulated, stake) = add_vote(&mut pools, 1024, 2);
        assert_eq!(accumulated, 3 * stake);

        // A jump beyond the expanded ring clears all previous accumulators.
        pools.purge(4096);
        let (accumulated, stake) = add_vote(&mut pools, 4096, 0);
        assert_eq!(accumulated, stake);
    }

    #[test]
    fn test_vote_pools_purge_across_ring_boundary() {
        let ctx = TestContext::new();
        let mut pools = VotePools::new(0);
        let len = pools.pools.len();
        pools.purge(len as Slot - 2);
        for slot in [len - 2, len - 1, len, len + 1] {
            add_vote(&ctx, &mut pools, slot as Slot, 0).unwrap();
        }

        pools.purge(len as Slot + 1);
        assert_eq!(pools.pools.len(), len);
        assert_eq!(pools.pools.iter().filter(|pool| pool.is_some()).count(), 1);
        let (accumulated, stake) = add_vote(&ctx, &mut pools, len as Slot + 1, 1).unwrap();
        assert_eq!(accumulated, 2 * stake);

        // Reuse each cleared position with a new slot and the original validator.
        for slot in [2 * len - 2, 2 * len - 1, 2 * len] {
            let (accumulated, stake) = add_vote(&ctx, &mut pools, slot as Slot, 0).unwrap();
            assert_eq!(accumulated, stake);
        }
        assert_eq!(pools.pools.len(), len);
    }

    #[test]
    fn test_vote_pools_purge_boundaries() {
        let ctx = TestContext::new();
        for full_purge in [false, true] {
            let mut pools = VotePools::new(10);
            let len = pools.pools.len();
            let last_slot = 10 + len as Slot - 1;
            add_vote(&ctx, &mut pools, 10, 0).unwrap();
            add_vote(&ctx, &mut pools, last_slot, 0).unwrap();

            let root = last_slot + Slot::from(full_purge);
            pools.purge(root);
            assert_eq!(pools.root_slot, root);
            assert_eq!(pools.pools.len(), len);
            assert_eq!(
                pools.pools.iter().filter(|pool| pool.is_some()).count(),
                usize::from(!full_purge),
            );
            let (accumulated, stake) = add_vote(&ctx, &mut pools, root, 1).unwrap();
            assert_eq!(accumulated, if full_purge { stake } else { 2 * stake });
        }
    }

    #[test]
    fn test_vote_pools_purge_unchanged_or_older_root() {
        let ctx = TestContext::new();
        let mut pools = VotePools::new(0);
        pools.purge(10);
        add_vote(&ctx, &mut pools, 10, 0).unwrap();
        add_vote(&ctx, &mut pools, 12, 0).unwrap();
        let offset = pools.offset;
        let len = pools.pools.len();

        for (rank, root) in [(1, 10), (2, 9)] {
            pools.purge(root);
            assert_eq!(pools.root_slot, 10);
            assert_eq!(pools.offset, offset);
            assert_eq!(pools.pools.len(), len);
            for slot in [10, 12] {
                let (accumulated, stake) = add_vote(&ctx, &mut pools, slot, rank).unwrap();
                assert_eq!(accumulated, (rank as u64 + 1) * stake);
            }
        }
    }

    #[test]
    fn test_vote_pools_growth_boundary() {
        let ctx = TestContext::new();
        let mut pools = VotePools::new(0);
        pools.purge(10);
        let len = pools.pools.len();
        let last_slot = 10 + len as Slot - 1;
        add_vote(&ctx, &mut pools, 10, 0).unwrap();
        add_vote(&ctx, &mut pools, last_slot, 0).unwrap();
        assert_eq!(pools.pools.len(), len);

        add_vote(&ctx, &mut pools, last_slot + 1, 0).unwrap();
        assert!(pools.pools.len() > len);
        for slot in [10, last_slot, last_slot + 1] {
            let (accumulated, stake) = add_vote(&ctx, &mut pools, slot, 1).unwrap();
            assert_eq!(accumulated, 2 * stake);
        }
    }

    #[test]
    fn test_vote_pools_old_vote_rejected() {
        let ctx = TestContext::new();
        let mut pools = VotePools::new(10);
        assert_eq!(
            add_vote(&ctx, &mut pools, 9, 0),
            Err(VotePoolError::OldVote {
                root_slot: 10,
                vote_slot: 9,
            }),
        );
        assert!(pools.pools.iter().all(Option::is_none));
    }
}
