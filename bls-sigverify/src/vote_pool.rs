use {
    agave_votor_messages::{
        reward_certificate::NUM_SLOTS_FOR_REWARD, unverified_vote_message::UnverifiedVoteMessage,
        vote::Vote,
    },
    bitvec::vec::BitVec,
    smallvec::SmallVec,
    solana_clock::Slot,
    solana_hash::Hash,
    std::collections::HashMap,
};

const MAX_NOTAR_FALLBACK_ENTRIES: usize = 3;

pub(crate) enum VotePoolError {
    Invalid,
    Duplicate,
}

fn default_bitvec(max_validators: usize) -> BitVec<u8> {
    BitVec::repeat(false, max_validators)
}

struct SlotEntry {
    skip: BitVec<u8>,
    skip_fallback: BitVec<u8>,
    finalize: BitVec<u8>,
    genesis: Vec<Option<Hash>>,
    notar: Vec<Option<Hash>>,
    notar_fallback: Vec<SmallVec<[Hash; MAX_NOTAR_FALLBACK_ENTRIES]>>,
}

impl SlotEntry {
    fn new(max_validators: usize) -> Self {
        Self {
            skip: default_bitvec(max_validators),
            skip_fallback: default_bitvec(max_validators),
            finalize: default_bitvec(max_validators),
            genesis: vec![None; max_validators],
            notar: vec![None; max_validators],
            notar_fallback: vec![SmallVec::new(); max_validators],
        }
    }

    fn try_add_vote(
        &mut self,
        msg: &UnverifiedVoteMessage,
        rank: usize,
        max_validators: usize,
    ) -> Result<(), VotePoolError> {
        debug_assert!(rank < max_validators);
        match &msg.vote {
            Vote::Skip(_) => {
                if self.notar[rank].is_some()
                    || self.finalize[rank]
                    || self.skip_fallback[rank]
                    || self.genesis[rank].is_some()
                {
                    return Err(VotePoolError::Invalid);
                }
                if self.skip.replace(rank, true) {
                    Err(VotePoolError::Duplicate)
                } else {
                    Ok(())
                }
            }
            Vote::SkipFallback(_) => {
                if self.finalize[rank] || self.skip[rank] || self.genesis[rank].is_some() {
                    return Err(VotePoolError::Invalid);
                }
                if self.skip_fallback.replace(rank, true) {
                    Err(VotePoolError::Duplicate)
                } else {
                    Ok(())
                }
            }
            Vote::Finalize(_) => {
                if self.skip[rank]
                    || self.skip_fallback[rank]
                    || !self.notar_fallback[rank].is_empty()
                    || self.genesis[rank].is_some()
                {
                    return Err(VotePoolError::Invalid);
                }
                if self.finalize.replace(rank, true) {
                    Err(VotePoolError::Duplicate)
                } else {
                    Ok(())
                }
            }
            Vote::Genesis(genesis) => {
                if self.skip[rank]
                    || self.skip_fallback[rank]
                    || self.finalize[rank]
                    || self.notar[rank].is_some()
                    || !self.notar_fallback[rank].is_empty()
                {
                    return Err(VotePoolError::Invalid);
                }
                match self.genesis[rank] {
                    None => {
                        self.genesis[rank] = Some(genesis.block.block_id);
                        Ok(())
                    }
                    Some(block_id) => {
                        if block_id == genesis.block.block_id {
                            Err(VotePoolError::Duplicate)
                        } else {
                            Err(VotePoolError::Invalid)
                        }
                    }
                }
            }
            Vote::Notarize(notar) => {
                if self.skip[rank]
                    || self.genesis[rank].is_some()
                    || self.notar_fallback[rank].contains(&notar.block.block_id)
                {
                    return Err(VotePoolError::Invalid);
                }
                match self.notar[rank] {
                    None => {
                        self.notar[rank] = Some(notar.block.block_id);
                        Ok(())
                    }
                    Some(block_id) => {
                        if block_id == notar.block.block_id {
                            Err(VotePoolError::Duplicate)
                        } else {
                            Err(VotePoolError::Invalid)
                        }
                    }
                }
            }
            Vote::NotarizeFallback(nf) => {
                if self.notar_fallback[rank].contains(&nf.block.block_id) {
                    return Err(VotePoolError::Duplicate);
                }
                if self.finalize[rank]
                    || self.genesis[rank].is_some()
                    || self.notar_fallback[rank].len() >= MAX_NOTAR_FALLBACK_ENTRIES
                {
                    return Err(VotePoolError::Invalid);
                }
                if let Some(block_id) = &self.notar[rank]
                    && block_id == &nf.block.block_id
                {
                    return Err(VotePoolError::Invalid);
                }
                self.notar_fallback[rank].push(nf.block.block_id);
                Ok(())
            }
        }
    }
}

#[derive(Default)]
pub(super) struct VotePool {
    entries: HashMap<Slot, SlotEntry>,
}

impl VotePool {
    pub(super) fn try_add_vote(
        &mut self,
        msg: &UnverifiedVoteMessage,
        rank: u16,
        max_validators: usize,
    ) -> Result<(), VotePoolError> {
        let rank = rank as usize;
        if rank >= max_validators {
            return Err(VotePoolError::Invalid);
        }
        let slot_entry = self
            .entries
            .entry(msg.vote.slot())
            .or_insert_with(|| SlotEntry::new(max_validators));
        slot_entry.try_add_vote(msg, rank, max_validators)
    }

    pub(super) fn prune(&mut self, root_slot: Slot) {
        // To support rewards, we need to keep older notar and skip votes.
        // Simpler to keep all votes for older slots.
        let slot_to_keep = root_slot.saturating_sub(NUM_SLOTS_FOR_REWARD);
        self.entries.retain(|slot, _| slot >= &slot_to_keep);
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, agave_votor_messages::consensus_message::Block, solana_bls_signatures::Keypair,
    };

    fn message(vote: Vote) -> UnverifiedVoteMessage {
        // VotePool inspects vote state, not signatures.
        UnverifiedVoteMessage {
            vote,
            signature: Keypair::new().sign(b"vote pool test").into(),
            shred_version: 0,
        }
    }

    #[test]
    fn duplicates_are_scoped_to_voter_and_slot() {
        let block = Block::new_unique(5);
        for vote in [
            Vote::new_skip_vote(5),
            Vote::new_skip_fallback_vote(5),
            Vote::new_finalization_vote(5),
            Vote::new_notarization_vote(block),
            Vote::new_notarization_fallback_vote(block),
            Vote::new_genesis_vote(block),
        ] {
            let mut pool = VotePool::default();
            let msg = message(vote);
            assert!(pool.try_add_vote(&msg, 0, 2).is_ok());
            assert!(matches!(
                pool.try_add_vote(&msg, 0, 2),
                Err(VotePoolError::Duplicate)
            ));
            assert!(pool.try_add_vote(&msg, 1, 2).is_ok());
        }
        let mut pool = VotePool::default();
        assert!(
            pool.try_add_vote(&message(Vote::new_skip_vote(5)), 0, 2)
                .is_ok()
        );
        assert!(
            pool.try_add_vote(&message(Vote::new_finalization_vote(6)), 0, 2)
                .is_ok()
        );
    }

    #[test]
    fn vote_combinations_in_both_arrival_orders() {
        let block = Block::new_unique(5);
        let votes = [
            Vote::new_skip_vote(5),
            Vote::new_skip_fallback_vote(5),
            Vote::new_finalization_vote(5),
            Vote::new_notarization_vote(block),
            Vote::new_notarization_fallback_vote(block),
            Vote::new_genesis_vote(block),
        ];
        // Allowed pairs: skip + notar fallback; skip fallback + notar;
        // skip fallback + notar fallback; finalize + notar.
        let allowed_pairs = [(0, 4), (1, 3), (1, 4), (2, 3)];
        for (i, first) in votes.iter().enumerate() {
            for (j, second) in votes.iter().enumerate() {
                if i == j {
                    continue;
                }
                let mut pool = VotePool::default();
                assert!(pool.try_add_vote(&message(*first), 0, 2).is_ok());
                let result = pool.try_add_vote(&message(*second), 0, 2);
                if allowed_pairs.contains(&(i.min(j), i.max(j))) {
                    assert!(result.is_ok(), "{first:?} then {second:?}");
                } else {
                    assert!(
                        matches!(result, Err(VotePoolError::Invalid)),
                        "{first:?} then {second:?}"
                    );
                }
                // Rejecting or accepting another vote must preserve the original vote.
                assert!(matches!(
                    pool.try_add_vote(&message(*first), 0, 2),
                    Err(VotePoolError::Duplicate)
                ));
            }
        }
    }

    #[test]
    fn conflicting_block_hashes_and_distinct_fallbacks() {
        let block1 = Block::new_unique(5);
        let block2 = Block::new_unique(5);
        for (first, second) in [
            (
                Vote::new_notarization_vote(block1),
                Vote::new_notarization_vote(block2),
            ),
            (
                Vote::new_genesis_vote(block1),
                Vote::new_genesis_vote(block2),
            ),
        ] {
            let mut pool = VotePool::default();
            assert!(pool.try_add_vote(&message(first), 0, 2).is_ok());
            assert!(matches!(
                pool.try_add_vote(&message(second), 0, 2),
                Err(VotePoolError::Invalid)
            ));
            assert!(matches!(
                pool.try_add_vote(&message(first), 0, 2),
                Err(VotePoolError::Duplicate)
            ));
        }
        let notar = message(Vote::new_notarization_vote(block1));
        let fallback = message(Vote::new_notarization_fallback_vote(block2));
        for (first, second) in [(&notar, &fallback), (&fallback, &notar)] {
            let mut pool = VotePool::default();
            assert!(pool.try_add_vote(first, 0, 2).is_ok());
            assert!(pool.try_add_vote(second, 0, 2).is_ok());
        }
    }

    #[test]
    fn notar_fallback_limit_counts_distinct_blocks_per_voter_and_slot() {
        let mut pool = VotePool::default();
        let votes = (0..4)
            .map(|_| message(Vote::new_unique_notar_fallback(5)))
            .collect::<Vec<_>>();
        for msg in &votes[..3] {
            assert!(pool.try_add_vote(msg, 0, 2).is_ok());
            assert!(matches!(
                pool.try_add_vote(msg, 0, 2),
                Err(VotePoolError::Duplicate)
            ));
        }
        assert!(matches!(
            pool.try_add_vote(&votes[3], 0, 2),
            Err(VotePoolError::Invalid)
        ));
        assert!(matches!(
            pool.try_add_vote(&votes[0], 0, 2),
            Err(VotePoolError::Duplicate)
        ));
        assert!(pool.try_add_vote(&votes[3], 1, 2).is_ok());
        assert!(
            pool.try_add_vote(&message(Vote::new_unique_notar_fallback(6)), 0, 2)
                .is_ok()
        );
    }

    #[test]
    fn pruning_preserves_reward_window_boundary() {
        let mut pool = VotePool::default();
        let votes = [0, 9, 10, 11].map(|slot| message(Vote::new_skip_vote(slot)));
        for msg in &votes {
            assert!(pool.try_add_vote(msg, 0, 2).is_ok());
        }
        pool.prune(0);
        assert!(matches!(
            pool.try_add_vote(&votes[0], 0, 2),
            Err(VotePoolError::Duplicate)
        ));
        pool.prune(NUM_SLOTS_FOR_REWARD + 10);
        for msg in &votes[..2] {
            assert!(pool.try_add_vote(msg, 0, 2).is_ok());
        }
        for msg in &votes[2..] {
            assert!(matches!(
                pool.try_add_vote(msg, 0, 2),
                Err(VotePoolError::Duplicate)
            ));
        }
        pool.prune(NUM_SLOTS_FOR_REWARD + 11);
        assert!(pool.try_add_vote(&votes[2], 0, 2).is_ok());
        assert!(matches!(
            pool.try_add_vote(&votes[3], 0, 2),
            Err(VotePoolError::Duplicate)
        ));
    }

    #[test]
    fn invalid_rank_does_not_record_vote() {
        let mut pool = VotePool::default();
        let msg = message(Vote::new_skip_vote(5));
        assert!(matches!(
            pool.try_add_vote(&msg, 2, 2),
            Err(VotePoolError::Invalid)
        ));
        assert!(matches!(
            pool.try_add_vote(&msg, u16::MAX, 2),
            Err(VotePoolError::Invalid)
        ));
        assert!(pool.try_add_vote(&msg, 1, 2).is_ok());
    }
}
