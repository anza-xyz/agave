use {
    crate::common::Stake,
    agave_votor_messages::consensus_message::VoteMessage,
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    std::collections::{BTreeMap, BTreeSet},
};

/// There are two types of vote pools:
/// - SimpleVotePool: Tracks all votes of a specfic vote type made by validators for some slot N, but only one vote per block.
/// - DuplicateBlockVotePool: Tracks all votes of a specfic vote type made by validators for some slot N,
///   but allows votes for different blocks by the same validator. Only relevant for VotePool's that are of type
///   Notarization or NotarizationFallback
pub(super) enum VotePool {
    SimpleVotePool(SimpleVotePool),
    DuplicateBlockVotePool(DuplicateBlockVotePool),
}

#[derive(Default)]
pub(super) struct SimpleVotePool {
    votes: Vec<VoteMessage>,
    total_stake: Stake,
    prev_voted_validators: BTreeSet<Pubkey>,
}

impl SimpleVotePool {
    pub(super) fn add_vote(
        &mut self,
        validator_vote_key: Pubkey,
        validator_stake: Stake,
        vote: VoteMessage,
    ) -> Option<Stake> {
        if !self.prev_voted_validators.insert(validator_vote_key) {
            return None;
        }
        self.votes.push(vote);
        self.total_stake = self.total_stake.saturating_add(validator_stake);
        Some(self.total_stake)
    }

    pub(super) fn votes(&self) -> &[VoteMessage] {
        &self.votes
    }

    pub(super) fn total_stake(&self) -> Stake {
        self.total_stake
    }

    pub(super) fn has_prev_validator_vote(&self, validator_vote_key: &Pubkey) -> bool {
        self.prev_voted_validators.contains(validator_vote_key)
    }
}

#[derive(Default)]
struct VoteEntry {
    votes: Vec<VoteMessage>,
    total_stake_by_key: Stake,
}

pub(super) struct DuplicateBlockVotePool {
    max_entries_per_pubkey: usize,
    vote_entries: BTreeMap<Hash, VoteEntry>,
    prev_voted_block_ids: BTreeMap<Pubkey, BTreeSet<Hash>>,
}

impl DuplicateBlockVotePool {
    pub(super) fn new(max_entries_per_pubkey: usize) -> Self {
        Self {
            max_entries_per_pubkey,
            vote_entries: BTreeMap::new(),
            prev_voted_block_ids: BTreeMap::new(),
        }
    }

    pub(super) fn add_vote(
        &mut self,
        validator_vote_key: Pubkey,
        validator_stake: Stake,
        vote: VoteMessage,
    ) -> Option<Stake> {
        let block_id = *vote.vote.block_id().unwrap();
        // Check whether the validator_vote_key already used the same voted_block_id or exceeded max_entries_per_pubkey
        // If so, return false, otherwise add the voted_block_id to the prev_votes
        let prev_voted_block_ids = self
            .prev_voted_block_ids
            .entry(validator_vote_key)
            .or_default();
        if prev_voted_block_ids.contains(&block_id)
            || prev_voted_block_ids.len() >= self.max_entries_per_pubkey
        {
            return None;
        }
        prev_voted_block_ids.insert(block_id);

        let vote_entry = self.vote_entries.entry(block_id).or_default();
        vote_entry.votes.push(vote);
        vote_entry.total_stake_by_key = vote_entry
            .total_stake_by_key
            .saturating_add(validator_stake);
        Some(vote_entry.total_stake_by_key)
    }

    pub(super) fn total_stake_by_block_id(&self, block_id: &Hash) -> Stake {
        self.vote_entries
            .get(block_id)
            .map_or(0, |vote_entries| vote_entries.total_stake_by_key)
    }

    pub(super) fn votes(&self, block_id: &Hash) -> Option<&[VoteMessage]> {
        self.vote_entries
            .get(block_id)
            .map(|entry| entry.votes.as_slice())
    }

    pub(super) fn has_prev_validator_vote_for_block(
        &self,
        validator_vote_key: &Pubkey,
        block_id: &Hash,
    ) -> bool {
        self.prev_voted_block_ids
            .get(validator_vote_key)
            .is_some_and(|vs| vs.contains(block_id))
    }

    pub(super) fn has_prev_validator_vote(&self, validator_vote_key: &Pubkey) -> bool {
        self.prev_voted_block_ids.contains_key(validator_vote_key)
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        agave_votor_messages::{consensus_message::VoteMessage, vote::Vote},
        solana_bls_signatures::Signature as BLSSignature,
    };

    #[test]
    fn test_skip_vote_pool() {
        let mut vote_pool = SimpleVotePool::default();
        let vote = Vote::new_skip_vote(5);
        let vote_message = VoteMessage {
            vote,
            signature: BLSSignature::default(),
            rank: 1,
        };
        let my_pubkey = Pubkey::new_unique();

        assert_eq!(vote_pool.add_vote(my_pubkey, 10, vote_message), Some(10));
        assert_eq!(vote_pool.total_stake(), 10);

        // Adding the same key again should fail
        assert_eq!(vote_pool.add_vote(my_pubkey, 10, vote_message), None);
        assert_eq!(vote_pool.total_stake(), 10);

        // Adding a different key should succeed
        let new_pubkey = Pubkey::new_unique();
        assert_eq!(vote_pool.add_vote(new_pubkey, 60, vote_message), Some(70));
        assert_eq!(vote_pool.total_stake(), 70);
    }

    #[test]
    fn test_notarization_pool() {
        let mut vote_pool = DuplicateBlockVotePool::new(1);
        let my_pubkey = Pubkey::new_unique();
        let block_id = Hash::new_unique();
        let vote = Vote::new_notarization_vote(3, block_id);
        let vote = VoteMessage {
            vote,
            signature: BLSSignature::default(),
            rank: 1,
        };
        assert_eq!(vote_pool.add_vote(my_pubkey, 10, vote), Some(10));
        assert_eq!(vote_pool.total_stake_by_block_id(&block_id), 10);

        // Adding the same key again should fail
        assert_eq!(vote_pool.add_vote(my_pubkey, 10, vote), None);

        // Adding a different bankhash should fail
        assert_eq!(vote_pool.add_vote(my_pubkey, 10, vote), None);

        // Adding a different key should succeed
        let new_pubkey = Pubkey::new_unique();
        assert_eq!(vote_pool.add_vote(new_pubkey, 60, vote), Some(70));
        assert_eq!(vote_pool.total_stake_by_block_id(&block_id), 70);
    }

    #[test]
    fn test_notarization_fallback_pool() {
        agave_logger::setup();
        let mut vote_pool = DuplicateBlockVotePool::new(3);
        let my_pubkey = Pubkey::new_unique();

        let votes = (0..4)
            .map(|_| {
                let vote = Vote::new_notarization_fallback_vote(7, Hash::new_unique());
                VoteMessage {
                    vote,
                    signature: BLSSignature::default(),
                    rank: 1,
                }
            })
            .collect::<Vec<_>>();

        // Adding the first 3 votes should succeed, but total_stake should remain at 10
        for vote in votes.iter().take(3).cloned() {
            assert_eq!(vote_pool.add_vote(my_pubkey, 10, vote), Some(10));
            assert_eq!(
                vote_pool.total_stake_by_block_id(vote.vote.block_id().unwrap()),
                10
            );
        }
        // Adding the 4th vote should fail
        assert_eq!(vote_pool.add_vote(my_pubkey, 10, votes[3]), None);
        assert_eq!(
            vote_pool.total_stake_by_block_id(votes[3].vote.block_id().unwrap()),
            0
        );

        // Adding a different key should succeed
        let new_pubkey = Pubkey::new_unique();
        for vote in votes.iter().skip(1).take(2).cloned() {
            assert_eq!(vote_pool.add_vote(new_pubkey, 60, vote), Some(70));
            assert_eq!(
                vote_pool.total_stake_by_block_id(vote.vote.block_id().unwrap()),
                70
            );
        }

        // The new key only added 2 votes, so adding block_ids[3] should succeed
        assert_eq!(vote_pool.add_vote(new_pubkey, 60, votes[3]), Some(60));
        assert_eq!(
            vote_pool.total_stake_by_block_id(votes[3].vote.block_id().unwrap()),
            60
        );

        // Now if adding the same key again, it should fail
        assert_eq!(vote_pool.add_vote(new_pubkey, 60, votes[0]), None);
    }
}
