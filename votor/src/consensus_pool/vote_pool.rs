use {
    crate::{common::Stake, consensus_pool::vote_certificate_builder::VoteCertificateBuilder},
    agave_votor_messages::consensus_message::VoteMessage,
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    std::collections::{HashMap, HashSet},
};

#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct VoteEntry {
    pub(crate) transactions: Vec<VoteMessage>,
    pub(crate) total_stake_by_key: Stake,
}

#[allow(dead_code)]
impl VoteEntry {
    pub fn new() -> Self {
        Self {
            transactions: Vec::new(),
            total_stake_by_key: 0,
        }
    }
}

#[allow(dead_code)]
pub(crate) trait VotePool {
    fn total_stake(&self) -> Stake;
    fn has_prev_validator_vote(&self, validator_vote_key: &Pubkey) -> bool;
}

#[allow(dead_code)]
/// There are two types of vote pools:
/// - SimpleVotePool: Tracks all votes of a specfic vote type made by validators for some slot N, but only one vote per block.
/// - DuplicateBlockVotePool: Tracks all votes of a specfic vote type made by validators for some slot N,
///   but allows votes for different blocks by the same validator. Only relevant for VotePool's that are of type
///   Notarization or NotarizationFallback
pub(crate) enum VotePoolType {
    SimpleVotePool(SimpleVotePool),
    DuplicateBlockVotePool(DuplicateBlockVotePool),
}

pub(crate) struct SimpleVotePool {
    /// Tracks all votes of a specfic vote type made by validators for some slot N.
    pub(crate) vote_entry: VoteEntry,
    prev_voted_validators: HashSet<Pubkey>,
}

#[allow(dead_code)]
impl SimpleVotePool {
    pub fn new() -> Self {
        Self {
            vote_entry: VoteEntry::new(),
            prev_voted_validators: HashSet::new(),
        }
    }

    pub fn add_vote(
        &mut self,
        validator_vote_key: &Pubkey,
        validator_stake: Stake,
        transaction: &VoteMessage,
    ) -> Option<Stake> {
        if self.prev_voted_validators.contains(validator_vote_key) {
            return None;
        }
        self.prev_voted_validators.insert(*validator_vote_key);
        self.vote_entry.transactions.push(*transaction);
        self.vote_entry.total_stake_by_key = self
            .vote_entry
            .total_stake_by_key
            .saturating_add(validator_stake);
        Some(self.vote_entry.total_stake_by_key)
    }

    pub fn add_to_certificate(&self, output: &mut VoteCertificateBuilder) {
        output
            .aggregate(&self.vote_entry.transactions)
            .expect("Incoming vote message signatures are assumed to be valid")
    }
}

impl VotePool for SimpleVotePool {
    fn total_stake(&self) -> Stake {
        self.vote_entry.total_stake_by_key
    }
    fn has_prev_validator_vote(&self, validator_vote_key: &Pubkey) -> bool {
        self.prev_voted_validators.contains(validator_vote_key)
    }
}

pub(crate) struct DuplicateBlockVotePool {
    max_entries_per_pubkey: usize,
    pub(crate) votes: HashMap<Hash, VoteEntry>,
    total_stake: Stake,
    prev_voted_block_ids: HashMap<Pubkey, Vec<Hash>>,
}

#[allow(dead_code)]
impl DuplicateBlockVotePool {
    pub fn new(max_entries_per_pubkey: usize) -> Self {
        Self {
            max_entries_per_pubkey,
            votes: HashMap::new(),
            total_stake: 0,
            prev_voted_block_ids: HashMap::new(),
        }
    }

    pub fn add_vote(
        &mut self,
        validator_vote_key: &Pubkey,
        voted_block_id: Hash,
        transaction: &VoteMessage,
        validator_stake: Stake,
    ) -> Option<Stake> {
        // Check whether the validator_vote_key already used the same voted_block_id or exceeded max_entries_per_pubkey
        // If so, return false, otherwise add the voted_block_id to the prev_votes
        let prev_voted_block_ids = self
            .prev_voted_block_ids
            .entry(*validator_vote_key)
            .or_default();
        if prev_voted_block_ids.contains(&voted_block_id) {
            return None;
        }
        let inserted_first_time = prev_voted_block_ids.is_empty();
        if prev_voted_block_ids.len() >= self.max_entries_per_pubkey {
            return None;
        }
        prev_voted_block_ids.push(voted_block_id);

        let vote_entry = self
            .votes
            .entry(voted_block_id)
            .or_insert_with(VoteEntry::new);
        vote_entry.transactions.push(*transaction);
        vote_entry.total_stake_by_key = vote_entry
            .total_stake_by_key
            .saturating_add(validator_stake);

        if inserted_first_time {
            self.total_stake = self.total_stake.saturating_add(validator_stake);
        }
        Some(vote_entry.total_stake_by_key)
    }

    pub fn total_stake_by_block_id(&self, block_id: &Hash) -> Stake {
        self.votes
            .get(block_id)
            .map_or(0, |vote_entries| vote_entries.total_stake_by_key)
    }

    pub fn add_to_certificate(&self, block_id: &Hash, output: &mut VoteCertificateBuilder) {
        if let Some(vote_entries) = self.votes.get(block_id) {
            output
                .aggregate(&vote_entries.transactions)
                .expect("Incoming vote message signatures are assumed to be valid")
        }
    }

    pub fn has_prev_validator_vote_for_block(
        &self,
        validator_vote_key: &Pubkey,
        block_id: &Hash,
    ) -> bool {
        self.prev_voted_block_ids
            .get(validator_vote_key)
            .is_some_and(|vs| vs.contains(block_id))
    }
}

impl VotePool for DuplicateBlockVotePool {
    fn total_stake(&self) -> Stake {
        self.total_stake
    }
    fn has_prev_validator_vote(&self, validator_vote_key: &Pubkey) -> bool {
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
        let mut vote_pool = SimpleVotePool::new();
        let vote = Vote::new_skip_vote(5);
        let transaction = VoteMessage {
            vote,
            signature: BLSSignature::default(),
            rank: 1,
        };
        let my_pubkey = Pubkey::new_unique();

        assert_eq!(vote_pool.add_vote(&my_pubkey, 10, &transaction), Some(10));
        assert_eq!(vote_pool.total_stake(), 10);

        // Adding the same key again should fail
        assert_eq!(vote_pool.add_vote(&my_pubkey, 10, &transaction), None);
        assert_eq!(vote_pool.total_stake(), 10);

        // Adding a different key should succeed
        let new_pubkey = Pubkey::new_unique();
        assert_eq!(vote_pool.add_vote(&new_pubkey, 60, &transaction), Some(70));
        assert_eq!(vote_pool.total_stake(), 70);
    }

    #[test]
    fn test_notarization_pool() {
        let mut vote_pool = DuplicateBlockVotePool::new(1);
        let my_pubkey = Pubkey::new_unique();
        let block_id = Hash::new_unique();
        let vote = Vote::new_notarization_vote(3, block_id);
        let transaction = VoteMessage {
            vote,
            signature: BLSSignature::default(),
            rank: 1,
        };
        assert_eq!(
            vote_pool.add_vote(&my_pubkey, block_id, &transaction, 10),
            Some(10)
        );
        assert_eq!(vote_pool.total_stake(), 10);
        assert_eq!(vote_pool.total_stake_by_block_id(&block_id), 10);

        // Adding the same key again should fail
        assert_eq!(
            vote_pool.add_vote(&my_pubkey, block_id, &transaction, 10),
            None
        );
        assert_eq!(vote_pool.total_stake(), 10);

        // Adding a different bankhash should fail
        assert_eq!(
            vote_pool.add_vote(&my_pubkey, block_id, &transaction, 10),
            None
        );
        assert_eq!(vote_pool.total_stake(), 10);

        // Adding a different key should succeed
        let new_pubkey = Pubkey::new_unique();
        assert_eq!(
            vote_pool.add_vote(&new_pubkey, block_id, &transaction, 60),
            Some(70)
        );
        assert_eq!(vote_pool.total_stake(), 70);
        assert_eq!(vote_pool.total_stake_by_block_id(&block_id), 70);
    }

    #[test]
    fn test_notarization_fallback_pool() {
        solana_logger::setup();
        let mut vote_pool = DuplicateBlockVotePool::new(3);
        let vote = Vote::new_notarization_fallback_vote(7, Hash::new_unique());
        let transaction = VoteMessage {
            vote,
            signature: BLSSignature::default(),
            rank: 1,
        };
        let my_pubkey = Pubkey::new_unique();

        let block_ids: Vec<Hash> = (0..4).map(|_| Hash::new_unique()).collect();

        // Adding the first 3 votes should succeed, but total_stake should remain at 10
        for block_id in &block_ids[0..3] {
            assert_eq!(
                vote_pool.add_vote(&my_pubkey, *block_id, &transaction, 10),
                Some(10)
            );
            assert_eq!(vote_pool.total_stake(), 10);
            assert_eq!(vote_pool.total_stake_by_block_id(block_id), 10);
        }
        // Adding the 4th vote should fail
        assert_eq!(
            vote_pool.add_vote(&my_pubkey, block_ids[3], &transaction, 10),
            None
        );
        assert_eq!(vote_pool.total_stake(), 10);
        assert_eq!(vote_pool.total_stake_by_block_id(&block_ids[3]), 0);

        // Adding a different key should succeed
        let new_pubkey = Pubkey::new_unique();
        for block_id in &block_ids[1..3] {
            assert_eq!(
                vote_pool.add_vote(&new_pubkey, *block_id, &transaction, 60),
                Some(70)
            );
            assert_eq!(vote_pool.total_stake(), 70);
            assert_eq!(vote_pool.total_stake_by_block_id(block_id), 70);
        }

        // The new key only added 2 votes, so adding block_ids[3] should succeed
        assert_eq!(
            vote_pool.add_vote(&new_pubkey, block_ids[3], &transaction, 60),
            Some(60)
        );
        assert_eq!(vote_pool.total_stake(), 70);
        assert_eq!(vote_pool.total_stake_by_block_id(&block_ids[3]), 60);

        // Now if adding the same key again, it should fail
        assert_eq!(
            vote_pool.add_vote(&new_pubkey, block_ids[0], &transaction, 60),
            None
        );
    }
}
