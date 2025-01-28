use {
    solana_sdk::clock::Slot,
    solana_vote_program::vote_state::{Lockout, VoteState, VoteState1_14_11, MAX_LOCKOUT_HISTORY},
    std::collections::VecDeque,
};

#[derive(Clone, Debug, PartialEq, Default)]
pub struct TowerVoteState {
    pub votes: VecDeque<Lockout>,
    pub root_slot: Option<Slot>,
}

impl TowerVoteState {
    pub fn tower(&self) -> Vec<Slot> {
        self.votes.iter().map(|v| v.slot()).collect()
    }

    pub fn last_lockout(&self) -> Option<&Lockout> {
        self.votes.back()
    }

    pub fn last_voted_slot(&self) -> Option<Slot> {
        self.last_lockout().map(|v| v.slot())
    }

    pub fn nth_recent_lockout(&self, position: usize) -> Option<&Lockout> {
        self.votes
            .len()
            .checked_sub(position.saturating_add(1))
            .and_then(|pos| self.votes.get(pos))
    }

    pub fn process_next_vote_slot(&mut self, next_vote_slot: Slot) {
        // Ignore votes for slots earlier than we already have votes for
        if self
            .last_voted_slot()
            .is_some_and(|last_voted_slot| next_vote_slot <= last_voted_slot)
        {
            return;
        }

        self.pop_expired_votes(next_vote_slot);

        // Once the stack is full, pop the oldest lockout and distribute rewards
        if self.votes.len() == MAX_LOCKOUT_HISTORY {
            let rooted_vote = self.votes.pop_front().unwrap();
            self.root_slot = Some(rooted_vote.slot());
        }
        self.votes.push_back(Lockout::new(next_vote_slot));
        self.double_lockouts();
    }

    // Pop all recent votes that are not locked out at the next vote slot.  This
    // allows validators to switch forks once their votes for another fork have
    // expired. This also allows validators continue voting on recent blocks in
    // the same fork without increasing lockouts.
    fn pop_expired_votes(&mut self, next_vote_slot: Slot) {
        while let Some(vote) = self.last_lockout() {
            if !vote.is_locked_out_at_slot(next_vote_slot) {
                self.votes.pop_back();
            } else {
                break;
            }
        }
    }

    fn double_lockouts(&mut self) {
        let stack_depth = self.votes.len();
        for (i, v) in self.votes.iter_mut().enumerate() {
            // Don't increase the lockout for this vote until we get more confirmations
            // than the max number of confirmations this vote has seen
            if stack_depth >
                i.checked_add(v.confirmation_count() as usize)
                    .expect("`confirmation_count` and tower_size should be bounded by `MAX_LOCKOUT_HISTORY`")
            {
                v.increase_confirmation_count(1);
            }
        }
    }
}

impl From<VoteState> for TowerVoteState {
    fn from(vote_state: VoteState) -> Self {
        let VoteState {
            votes, root_slot, ..
        } = vote_state;

        Self {
            votes: votes
                .into_iter()
                .map(|landed_vote| landed_vote.into())
                .collect(),
            root_slot,
        }
    }
}

impl From<VoteState1_14_11> for TowerVoteState {
    fn from(vote_state: VoteState1_14_11) -> Self {
        let VoteState1_14_11 {
            votes, root_slot, ..
        } = vote_state;

        Self { votes, root_slot }
    }
}

impl From<TowerVoteState> for VoteState1_14_11 {
    fn from(vote_state: TowerVoteState) -> Self {
        let TowerVoteState { votes, root_slot } = vote_state;

        VoteState1_14_11 {
            votes,
            root_slot,
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_vote_program::vote_state::INITIAL_LOCKOUT, std::collections::VecDeque};

    fn check_lockouts(vote_state: &TowerVoteState) {
        for (i, vote) in vote_state.votes.iter().enumerate() {
            let num_votes = vote_state
                .votes
                .len()
                .checked_sub(i)
                .expect("`i` is less than `vote_state.votes.len()`");
            assert_eq!(vote.lockout(), INITIAL_LOCKOUT.pow(num_votes as u32) as u64);
        }
    }

    #[test]
    fn test_basic_vote_state() {
        let mut vote_state = TowerVoteState::default();

        // Process initial vote
        vote_state.process_next_vote_slot(1);
        assert_eq!(vote_state.votes.len(), 1);
        assert_eq!(vote_state.votes[0].slot(), 1);
        assert_eq!(vote_state.votes[0].confirmation_count(), 1);
        assert_eq!(vote_state.root_slot, None);

        // Process second vote
        vote_state.process_next_vote_slot(2);
        assert_eq!(vote_state.votes.len(), 2);
        assert_eq!(vote_state.votes[0].slot(), 1);
        assert_eq!(vote_state.votes[0].confirmation_count(), 2);
        assert_eq!(vote_state.votes[1].slot(), 2);
        assert_eq!(vote_state.votes[1].confirmation_count(), 1);
    }

    #[test]
    fn test_vote_lockout() {
        let mut vote_state = TowerVoteState::default();

        // Fill up the vote history
        for i in 0..(MAX_LOCKOUT_HISTORY + 1) {
            vote_state.process_next_vote_slot(i as u64);
        }

        // Verify the earliest vote was popped and became the root
        assert_eq!(vote_state.votes.len(), MAX_LOCKOUT_HISTORY);
        assert_eq!(vote_state.root_slot, Some(0));
        check_lockouts(&vote_state);

        // Verify lockout counts are correct
        for (i, vote) in vote_state.votes.iter().enumerate() {
            let expected_count = MAX_LOCKOUT_HISTORY - i;
            assert_eq!(vote.confirmation_count(), expected_count as u32);
        }

        // One more vote that confirms the entire stack,
        // the root_slot should change to the
        // second vote
        let top_vote = vote_state.votes.front().unwrap().slot();
        let slot = vote_state.last_lockout().unwrap().last_locked_out_slot();
        vote_state.process_next_vote_slot(slot);
        assert_eq!(Some(top_vote), vote_state.root_slot);

        // Expire everything except the first vote
        let slot = vote_state.votes.front().unwrap().last_locked_out_slot();
        vote_state.process_next_vote_slot(slot);
        // First vote and new vote are both stored for a total of 2 votes
        assert_eq!(vote_state.votes.len(), 2);
    }

    #[test]
    fn test_vote_double_lockout_after_expiration() {
        let mut vote_state = TowerVoteState::default();

        for i in 0..3 {
            vote_state.process_next_vote_slot(i as u64);
        }

        check_lockouts(&vote_state);

        // Expire the third vote (which was a vote for slot 2). The height of the
        // vote stack is unchanged, so none of the previous votes should have
        // doubled in lockout
        vote_state.process_next_vote_slot((2 + INITIAL_LOCKOUT + 1) as u64);
        check_lockouts(&vote_state);

        // Vote again, this time the vote stack depth increases, so the votes should
        // double for everybody
        vote_state.process_next_vote_slot((2 + INITIAL_LOCKOUT + 2) as u64);
        check_lockouts(&vote_state);

        // Vote again, this time the vote stack depth increases, so the votes should
        // double for everybody
        vote_state.process_next_vote_slot((2 + INITIAL_LOCKOUT + 3) as u64);
        check_lockouts(&vote_state);
    }

    #[test]
    fn test_expire_multiple_votes() {
        let mut vote_state = TowerVoteState::default();

        for i in 0..3 {
            vote_state.process_next_vote_slot(i as u64);
        }

        assert_eq!(vote_state.votes[0].confirmation_count(), 3);

        // Expire the second and third votes
        let expire_slot = vote_state.votes[1].slot() + vote_state.votes[1].lockout() + 1;
        vote_state.process_next_vote_slot(expire_slot);
        assert_eq!(vote_state.votes.len(), 2);

        // Check that the old votes expired
        assert_eq!(vote_state.votes[0].slot(), 0);
        assert_eq!(vote_state.votes[1].slot(), expire_slot);

        // Process one more vote
        vote_state.process_next_vote_slot(expire_slot + 1);

        // Confirmation count for the older first vote should remain unchanged
        assert_eq!(vote_state.votes[0].confirmation_count(), 3);

        // The later votes should still have increasing confirmation counts
        assert_eq!(vote_state.votes[1].confirmation_count(), 2);
        assert_eq!(vote_state.votes[2].confirmation_count(), 1);
    }

    #[test]
    fn test_multiple_root_progress() {
        let mut vote_state = TowerVoteState::default();

        // Add enough votes to create first root
        for i in 0..(MAX_LOCKOUT_HISTORY + 1) {
            vote_state.process_next_vote_slot(i as u64);
        }
        assert_eq!(vote_state.root_slot, Some(0));

        // Add more votes to advance root
        vote_state.process_next_vote_slot(MAX_LOCKOUT_HISTORY as u64 + 1);
        assert_eq!(vote_state.root_slot, Some(1));

        vote_state.process_next_vote_slot(MAX_LOCKOUT_HISTORY as u64 + 2);
        assert_eq!(vote_state.root_slot, Some(2));
    }

    #[test]
    fn test_duplicate_vote() {
        let mut vote_state = TowerVoteState::default();

        // Process initial votes
        vote_state.process_next_vote_slot(1);
        vote_state.process_next_vote_slot(2);

        // Try duplicate vote
        vote_state.process_next_vote_slot(1);

        // Verify the vote state (duplicate should not affect anything)
        assert_eq!(vote_state.votes.len(), 2);
        assert_eq!(vote_state.votes[0].slot(), 1);
        assert_eq!(vote_state.votes[1].slot(), 2);

        // Try duplicate vote
        vote_state.process_next_vote_slot(2);

        // Verify the vote state (duplicate should not affect anything)
        assert_eq!(vote_state.votes.len(), 2);
        assert_eq!(vote_state.votes[0].slot(), 1);
        assert_eq!(vote_state.votes[1].slot(), 2);
    }

    #[test]
    fn test_vote_state_roots() {
        let mut vote_state = TowerVoteState {
            votes: VecDeque::new(),
            root_slot: Some(5), // Start with existing root
        };

        // Add votes after root
        vote_state.process_next_vote_slot(6);
        vote_state.process_next_vote_slot(7);

        // Verify votes after root are tracked
        assert_eq!(vote_state.votes.len(), 2);
        assert_eq!(vote_state.votes[0].slot(), 6);
        assert_eq!(vote_state.votes[1].slot(), 7);
        assert_eq!(vote_state.root_slot, Some(5));

        // Fill up vote history to advance root
        for i in 8..=(MAX_LOCKOUT_HISTORY as u64 + 8) {
            vote_state.process_next_vote_slot(i);
        }

        // Verify root has advanced
        assert!(vote_state.root_slot.unwrap() > 5);
    }
}
