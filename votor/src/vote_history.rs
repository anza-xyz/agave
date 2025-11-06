use {
    super::vote_history_storage::{
        Result, SavedVoteHistory, SavedVoteHistoryVersions, VoteHistoryStorage,
    },
    agave_votor_messages::{consensus_message::Block, vote::Vote},
    serde::{Deserialize, Serialize},
    solana_clock::Slot,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    std::collections::{hash_map::Entry, HashMap, HashSet},
    thiserror::Error,
};

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub enum VoteHistoryVersions {
    Current(VoteHistory),
}
impl VoteHistoryVersions {
    pub fn new_current(vote_history: VoteHistory) -> Self {
        Self::Current(vote_history)
    }

    pub fn convert_to_current(self) -> VoteHistory {
        match self {
            VoteHistoryVersions::Current(vote_history) => vote_history,
        }
    }
}

#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample),
    frozen_abi(digest = "5sT71PEL9bNaZhoQGjLwiMESWDRMMVmW1wMvtQpvZs5F")
)]
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Default)]
pub struct VoteHistory {
    /// The validator identity that cast votes
    pub node_pubkey: Pubkey,

    /// The slots which this node has cast either a notarization or skip vote
    voted: HashSet<Slot>,

    /// The blocks for which this node has cast a notarization vote
    /// In the format of slot, block_id, bank_hash
    voted_notar: HashMap<Slot, Hash>,

    /// The blocks for which this node has cast a notarization fallback
    /// vote in this slot
    voted_notar_fallback: HashMap<Slot, HashSet<Hash>>,

    /// The slots for which this node has cast a skip fallback vote
    voted_skip_fallback: HashSet<Slot>,

    /// The slots in which this node has cast at least one of:
    /// - `SkipVote`
    /// - `SkipFallback`
    /// - `NotarizeFallback`
    skipped: HashSet<Slot>,

    /// The slots for which this node has cast a finalization vote. This node
    /// will not cast any additional votes for these slots
    its_over: HashSet<Slot>,

    /// All votes cast for a `slot`, for use in refresh
    votes_cast: HashMap<Slot, Vec<Vote>>,

    /// Blocks which have a notarization certificate via the certificate pool
    notarized_blocks: HashSet<Block>,

    /// Slots which have a parent ready condition via the certificate pool
    parent_ready_slots: HashMap<Slot, HashSet<Block>>,

    /// The latest root set by the voting loop. The above structures will not
    /// contain votes for slots before `root`
    root: Slot,
}

impl VoteHistory {
    pub fn new(node_pubkey: Pubkey, root: Slot) -> Self {
        Self {
            node_pubkey,
            root,
            ..Self::default()
        }
    }

    /// Have we cast a notarization or skip vote for `slot`
    pub fn voted(&self, slot: Slot) -> bool {
        assert!(slot >= self.root);
        self.voted.contains(&slot)
    }

    /// The block for which we voted notarize in slot `slot`
    pub fn voted_notar(&self, slot: Slot) -> Option<Hash> {
        assert!(slot >= self.root);
        self.voted_notar.get(&slot).copied()
    }

    /// Whether we voted notarize fallback in `slot` for block `(block_id, bank_hash)`
    pub fn voted_notar_fallback(&self, slot: Slot, block_id: Hash) -> bool {
        assert!(slot >= self.root);
        self.voted_notar_fallback
            .get(&slot)
            .is_some_and(|v| v.contains(&block_id))
    }

    /// Whether we voted skip fallback for `slot`
    pub fn voted_skip_fallback(&self, slot: Slot) -> bool {
        assert!(slot >= self.root);
        self.voted_skip_fallback.contains(&slot)
    }

    /// Have we cast any skip vote variation for `slot`
    pub fn skipped(&self, slot: Slot) -> bool {
        assert!(slot >= self.root);
        self.skipped.contains(&slot)
    }

    /// Have we casted a finalization vote for `slot`
    pub fn its_over(&self, slot: Slot) -> bool {
        assert!(slot >= self.root);
        self.its_over.contains(&slot)
    }

    /// All votes cast since `slot` excluding `slot`, for use in
    /// refresh
    pub fn votes_cast_since(&self, slot: Slot) -> Vec<Vote> {
        self.votes_cast
            .iter()
            .filter(|(s, _)| s > &&slot)
            .flat_map(|(_, votes)| votes.iter())
            .cloned()
            .collect()
    }

    /// Have we casted a bad window vote for `slot`:
    /// - Skip
    /// - Notarize fallback
    /// - Skip fallback
    pub fn bad_window(&self, slot: Slot) -> bool {
        assert!(slot >= self.root);
        self.skipped.contains(&slot)
            || self.voted_notar_fallback.contains_key(&slot)
            || self.voted_skip_fallback.contains(&slot)
    }

    pub fn is_block_notarized(&self, block: &Block) -> bool {
        self.notarized_blocks.contains(block)
    }

    pub fn is_parent_ready(&self, slot: Slot, parent: &Block) -> bool {
        self.parent_ready_slots
            .get(&slot)
            .is_some_and(|ps| ps.contains(parent))
    }

    /// The latest root slot set by the voting loop
    pub fn root(&self) -> Slot {
        self.root
    }

    /// Add a new vote to the voting history
    pub fn add_vote(&mut self, vote: Vote) {
        assert!(vote.slot() >= self.root);
        // TODO: these assert!s are for my debugging, can consider removing
        // in final version
        match vote {
            Vote::Notarize(vote) => {
                assert!(self.voted.insert(vote.slot));
                assert!(self.voted_notar.insert(vote.slot, vote.block_id).is_none());
            }
            Vote::Finalize(vote) => {
                assert!(!self.skipped(vote.slot));
                self.its_over.insert(vote.slot);
            }
            Vote::Skip(vote) => {
                self.voted.insert(vote.slot);
                self.skipped.insert(vote.slot);
            }
            Vote::NotarizeFallback(vote) => {
                assert!(self.voted(vote.slot));
                assert!(!self.its_over(vote.slot));
                self.skipped.insert(vote.slot);
                self.voted_notar_fallback
                    .entry(vote.slot)
                    .or_default()
                    .insert(vote.block_id);
            }
            Vote::SkipFallback(vote) => {
                assert!(self.voted(vote.slot));
                assert!(!self.its_over(vote.slot));
                self.skipped.insert(vote.slot);
                self.voted_skip_fallback.insert(vote.slot);
            }
        }
        self.votes_cast.entry(vote.slot()).or_default().push(vote);
    }

    /// Add a new notarized block
    pub fn add_block_notarized(&mut self, block @ (slot, _): Block) {
        if slot < self.root {
            return;
        }
        self.notarized_blocks.insert(block);
    }

    /// Add a new parent ready slot
    ///
    /// Returns true if the insertion was successful and this was the
    /// first parent ready for this slot, indicating we should set timeouts.
    pub fn add_parent_ready(&mut self, slot: Slot, parent: Block) -> bool {
        if slot < self.root {
            return false;
        }
        match self.parent_ready_slots.entry(slot) {
            Entry::Occupied(mut entry) => {
                entry.get_mut().insert(parent);
                false
            }
            Entry::Vacant(entry) => {
                entry.insert(HashSet::from([parent]));
                true
            }
        }
    }

    pub fn highest_parent_ready_slot(&self) -> Option<Slot> {
        self.parent_ready_slots.keys().max().copied()
    }

    /// Sets the new root slot and cleans up outdated slots < `root`
    pub fn set_root(&mut self, root: Slot) {
        self.root = root;
        self.voted.retain(|s| *s >= root);
        self.voted_notar.retain(|s, _| *s >= root);
        self.voted_notar_fallback.retain(|s, _| *s >= root);
        self.voted_skip_fallback.retain(|s| *s >= root);
        self.skipped.retain(|s| *s >= root);
        self.its_over.retain(|s| *s >= root);
        self.votes_cast.retain(|s, _| *s >= root);
        self.notarized_blocks.retain(|(s, _)| *s >= root);
        self.parent_ready_slots.retain(|s, _| *s >= root);
    }

    #[allow(dead_code)]
    /// Save the vote history to `vote_history_storage` signed by `node_keypair`
    pub fn save(
        &self,
        vote_history_storage: &dyn VoteHistoryStorage,
        node_keypair: &Keypair,
    ) -> Result<()> {
        let saved_vote_history = SavedVoteHistory::new(self, node_keypair)?;
        vote_history_storage.store(&SavedVoteHistoryVersions::from(saved_vote_history))?;
        Ok(())
    }

    /// Restore the saved vote history from `vote_history_storage` for `node_pubkey`
    pub fn restore(
        vote_history_storage: &dyn VoteHistoryStorage,
        node_pubkey: &Pubkey,
    ) -> Result<Self> {
        vote_history_storage.load(node_pubkey)
    }
}

#[derive(Error, Debug)]
pub enum VoteHistoryError {
    #[error("IO Error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Serialization Error: {0}")]
    SerializeError(#[from] bincode::Error),

    #[error("The signature on the saved vote history is invalid")]
    InvalidSignature,

    #[error("The vote history does not match this validator: {0}")]
    WrongVoteHistory(String),

    #[error("The vote history is useless because of new hard fork: {0}")]
    HardFork(Slot),
}

impl VoteHistoryError {
    pub fn is_file_missing(&self) -> bool {
        if let VoteHistoryError::IoError(io_err) = &self {
            io_err.kind() == std::io::ErrorKind::NotFound
        } else {
            false
        }
    }
}

#[cfg(test)]
mod test {
    use {
        super::*, crate::vote_history_storage::FileVoteHistoryStorage,
        agave_votor_messages::vote::Vote, solana_signer::Signer, tempfile::TempDir,
    };

    // Votes cast since is kept in HashMap, so order is not guaranteed.
    // This function checks that the votes are the same, regardless of order.
    fn check_votes_cast_since(vote_history: &VoteHistory, slot: Slot, expected_votes: Vec<Vote>) {
        let votes = vote_history.votes_cast_since(slot);
        assert_eq!(votes.len(), expected_votes.len());
        // This is correct because expected_votes has no duplicates
        for vote in expected_votes {
            assert!(votes.contains(&vote));
        }
    }

    #[test]
    fn test_add_votes() {
        let mut vote_history = VoteHistory::new(Pubkey::new_unique(), 0);
        // No votes for now
        assert!(vote_history.votes_cast_since(0).is_empty());

        // Vote Notarize on slot 1
        let block_id_1 = Hash::new_unique();
        let vote_notarize_1 = Vote::new_notarization_vote(1, block_id_1);
        vote_history.add_vote(vote_notarize_1);
        assert!(vote_history.voted(1));
        assert!(!vote_history.its_over(1));
        check_votes_cast_since(&vote_history, 0, vec![vote_notarize_1]);
        assert_eq!(vote_history.voted_notar(1), Some(block_id_1));
        assert!(!vote_history.skipped(1));
        assert!(!vote_history.voted_notar_fallback(1, block_id_1));
        assert!(!vote_history.bad_window(1));

        // Vote Finalize on slot 1
        let vote_finalize_1 = Vote::new_finalization_vote(1);
        vote_history.add_vote(vote_finalize_1);
        assert!(vote_history.voted(1));
        assert!(vote_history.its_over(1));
        check_votes_cast_since(&vote_history, 0, vec![vote_notarize_1, vote_finalize_1]);
        assert!(!vote_history.bad_window(1));

        // Vote Skip on slot 2
        let vote_skip_2 = Vote::new_skip_vote(2);
        vote_history.add_vote(vote_skip_2);
        assert!(vote_history.voted(2));
        assert!(vote_history.skipped(2));
        check_votes_cast_since(
            &vote_history,
            0,
            vec![vote_notarize_1, vote_finalize_1, vote_skip_2],
        );
        assert_eq!(vote_history.voted_notar(2), None);
        assert!(!vote_history.its_over(2));
        assert!(vote_history.bad_window(2));

        // Now vote NotarizeFallback on slot 2
        let block_id_2 = Hash::new_unique();
        let vote_notarize_fallback_2 = Vote::new_notarization_fallback_vote(2, block_id_2);
        vote_history.add_vote(vote_notarize_fallback_2);
        assert!(vote_history.voted(2));
        assert!(vote_history.skipped(2));
        assert_eq!(vote_history.voted_notar(2), None);
        assert!(vote_history.voted_notar_fallback(2, block_id_2));
        check_votes_cast_since(
            &vote_history,
            0,
            vec![
                vote_notarize_1,
                vote_finalize_1,
                vote_skip_2,
                vote_notarize_fallback_2,
            ],
        );
        assert!(!vote_history.its_over(2));
        assert!(vote_history.bad_window(2));

        // Vote Notarize on slot 3
        let block_id_3 = Hash::new_unique();
        let vote_notarize_3 = Vote::new_notarization_vote(3, block_id_3);
        vote_history.add_vote(vote_notarize_3);
        assert!(vote_history.voted(3));
        assert!(!vote_history.skipped(3));
        assert_eq!(vote_history.voted_notar(3), Some(block_id_3));
        assert!(!vote_history.voted_notar_fallback(3, block_id_3));
        check_votes_cast_since(
            &vote_history,
            0,
            vec![
                vote_notarize_1,
                vote_finalize_1,
                vote_skip_2,
                vote_notarize_fallback_2,
                vote_notarize_3,
            ],
        );
        assert!(!vote_history.its_over(3));
        assert!(!vote_history.bad_window(3));

        // Now vote SkipFallback on slot 3
        let vote_skip_fallback_3 = Vote::new_skip_fallback_vote(3);
        vote_history.add_vote(vote_skip_fallback_3);
        assert!(vote_history.voted(3));
        assert!(vote_history.skipped(3));
        assert_eq!(vote_history.voted_notar(3), Some(block_id_3));
        assert!(!vote_history.voted_notar_fallback(3, block_id_3));
        assert!(vote_history.voted_skip_fallback(3));
        check_votes_cast_since(
            &vote_history,
            0,
            vec![
                vote_notarize_1,
                vote_finalize_1,
                vote_skip_2,
                vote_notarize_fallback_2,
                vote_notarize_3,
                vote_skip_fallback_3,
            ],
        );
        assert!(!vote_history.its_over(3));
        assert!(vote_history.bad_window(3));

        // Set root on 2
        vote_history.set_root(2);
        assert_eq!(vote_history.root(), 2);
        check_votes_cast_since(
            &vote_history,
            0,
            vec![
                vote_skip_2,
                vote_notarize_fallback_2,
                vote_notarize_3,
                vote_skip_fallback_3,
            ],
        );
        // set_root doesn't automatically set its_over to true
        assert!(!vote_history.its_over(2));
    }

    #[test]
    fn test_add_notarized_blocks() {
        let mut vote_history = VoteHistory::new(Pubkey::new_unique(), 0);
        let block_1 = (1, Hash::new_unique());
        assert!(!vote_history.is_block_notarized(&block_1));
        vote_history.add_block_notarized(block_1);
        assert!(vote_history.is_block_notarized(&block_1));

        let block_2 = (2, Hash::new_unique());
        assert!(!vote_history.is_block_notarized(&block_2));
        vote_history.add_block_notarized(block_2);
        assert!(vote_history.is_block_notarized(&block_2));

        vote_history.set_root(2);
        assert_eq!(vote_history.root(), 2);
        assert!(!vote_history.is_block_notarized(&block_1));
        assert!(vote_history.is_block_notarized(&block_2));

        // Adding a block before root silently returns
        vote_history.add_block_notarized(block_1);
        assert!(!vote_history.is_block_notarized(&block_1));
    }

    #[test]
    fn test_add_parent_ready() {
        let mut vote_history = VoteHistory::new(Pubkey::new_unique(), 0);
        assert_eq!(vote_history.highest_parent_ready_slot(), None);
        let block_id_0 = (0, Hash::new_unique());
        vote_history.add_parent_ready(1, block_id_0);
        assert!(vote_history.is_parent_ready(1, &block_id_0));
        assert_eq!(vote_history.highest_parent_ready_slot(), Some(1));

        vote_history.set_root(1);
        assert_eq!(vote_history.root(), 1);
        assert!(vote_history.is_parent_ready(1, &block_id_0));
        assert_eq!(vote_history.highest_parent_ready_slot(), Some(1));

        // Add parent ready for slot 2
        let block_id_2_0 = (1, Hash::new_unique());
        let block_id_2_1 = (1, Hash::new_unique());
        assert!(vote_history.add_parent_ready(2, block_id_2_0));
        assert!(vote_history.is_parent_ready(2, &block_id_2_0));
        assert_eq!(vote_history.highest_parent_ready_slot(), Some(2));
        assert!(!vote_history.add_parent_ready(2, block_id_2_1));
        assert!(vote_history.is_parent_ready(2, &block_id_2_1));
        assert!(!vote_history.add_parent_ready(2, block_id_0));
        assert!(vote_history.is_parent_ready(2, &block_id_0));

        // Set root to 2
        vote_history.set_root(2);
        assert_eq!(vote_history.root(), 2);
        assert!(!vote_history.is_parent_ready(1, &block_id_0));
        assert!(vote_history.is_parent_ready(2, &block_id_2_0));
        assert!(vote_history.is_parent_ready(2, &block_id_2_1));
        assert!(vote_history.is_parent_ready(2, &block_id_0));
        assert_eq!(vote_history.highest_parent_ready_slot(), Some(2));

        // Adding a parent ready for slot before root silently returns false
        assert!(!vote_history.add_parent_ready(1, block_id_0));
    }

    #[test]
    fn test_save_and_restore() {
        let node_keypair = Keypair::new();
        let mut vote_history = VoteHistory::new(node_keypair.pubkey(), 0);
        let tmp_dir = TempDir::new().unwrap();
        let vote_history_storage = FileVoteHistoryStorage::new(tmp_dir.path().to_path_buf());

        // Add Notarize on 1 and Skip on 2
        let vote_1 = Vote::new_notarization_vote(1, Hash::new_unique());
        let vote_2 = Vote::new_skip_vote(2);
        vote_history.add_vote(vote_1);
        vote_history.add_vote(vote_2);

        // Save to storage
        vote_history
            .save(&vote_history_storage, &node_keypair)
            .unwrap();
        // Restore from storage
        let restored_vote_history =
            VoteHistory::restore(&vote_history_storage, &node_keypair.pubkey())
                .ok()
                .unwrap();
        check_votes_cast_since(&restored_vote_history, 0, vec![vote_1, vote_2]);
        assert_eq!(restored_vote_history, vote_history);

        // Save should fail if you give wrong keypair
        let error = vote_history
            .save(&vote_history_storage, &Keypair::new())
            .err()
            .unwrap();
        assert!(matches!(error, VoteHistoryError::WrongVoteHistory(_)));
        assert!(!error.is_file_missing());

        // Restore should fail if you give wrong pubkey
        let error = VoteHistory::restore(&vote_history_storage, &Pubkey::new_unique())
            .err()
            .unwrap();
        assert!(matches!(error, VoteHistoryError::IoError(_)));
        assert!(error.is_file_missing());
    }
}
