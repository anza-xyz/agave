//! Vote state handler API.
//!
//! Wraps the vote state behind a "handler" API to support converting from an
//! existing vote state version to whichever version is the target (or
//! "current") vote state version.
//!
//! The program must be generic over whichever vote state version is the
//! target, since at compile time the target version is not known (can be
//! changed with a feature gate). For this reason, the handler offers a
//! getter and setter API around vote state, for all operations required by the
//! vote program.

#[cfg(test)]
use solana_vote_interface::state::Lockout;
use {
    solana_clock::{Clock, Epoch, Slot},
    solana_instruction::error::InstructionError,
    solana_pubkey::Pubkey,
    solana_transaction_context::BorrowedInstructionAccount,
    solana_vote_interface::{
        error::VoteError,
        state::{LandedVote, VoteInit, VoteState1_14_11, VoteStateV3, VoteStateVersions},
    },
    std::collections::VecDeque,
};

/// Trait defining the interface for vote state operations.
pub trait VoteStateHandle {
    fn is_uninitialized(&self) -> bool;

    fn authorized_withdrawer(&self) -> &Pubkey;

    fn set_authorized_withdrawer(&mut self, authorized_withdrawer: Pubkey);

    fn set_new_authorized_voter<F>(
        &mut self,
        authorized_pubkey: &Pubkey,
        current_epoch: Epoch,
        target_epoch: Epoch,
        verify: F,
    ) -> Result<(), InstructionError>
    where
        F: Fn(Pubkey) -> Result<(), InstructionError>;

    fn get_and_update_authorized_voter(
        &mut self,
        current_epoch: Epoch,
    ) -> Result<Pubkey, InstructionError>;

    fn commission(&self) -> u8;

    fn set_commission(&mut self, commission: u8);

    fn node_pubkey(&self) -> &Pubkey;

    fn set_node_pubkey(&mut self, node_pubkey: Pubkey);

    fn votes(&self) -> &VecDeque<LandedVote>;

    fn votes_mut(&mut self) -> &mut VecDeque<LandedVote>;

    fn set_votes(&mut self, votes: VecDeque<LandedVote>);

    fn contains_slot(&self, slot: Slot) -> bool;

    fn last_voted_slot(&self) -> Option<Slot>;

    fn root_slot(&self) -> Option<Slot>;

    fn set_root_slot(&mut self, root_slot: Option<Slot>);

    fn current_epoch(&self) -> Epoch;

    fn epoch_credits_last(&self) -> Option<&(Epoch, u64, u64)>;

    fn credits_for_vote_at_index(&self, index: usize) -> u64;

    fn increment_credits(&mut self, epoch: Epoch, credits: u64);

    fn process_timestamp(&mut self, slot: Slot, timestamp: i64) -> Result<(), VoteError>;

    fn process_next_vote_slot(&mut self, next_vote_slot: Slot, epoch: Epoch, current_slot: Slot);

    fn set_vote_account_state(
        self,
        vote_account: &mut BorrowedInstructionAccount,
    ) -> Result<(), InstructionError>;
}

impl VoteStateHandle for VoteStateV3 {
    fn is_uninitialized(&self) -> bool {
        self.authorized_voters.is_empty()
    }

    fn authorized_withdrawer(&self) -> &Pubkey {
        &self.authorized_withdrawer
    }

    fn set_authorized_withdrawer(&mut self, authorized_withdrawer: Pubkey) {
        self.authorized_withdrawer = authorized_withdrawer;
    }

    fn set_new_authorized_voter<F>(
        &mut self,
        authorized_pubkey: &Pubkey,
        current_epoch: Epoch,
        target_epoch: Epoch,
        verify: F,
    ) -> Result<(), InstructionError>
    where
        F: Fn(Pubkey) -> Result<(), InstructionError>,
    {
        let epoch_authorized_voter = self.get_and_update_authorized_voter(current_epoch)?;
        verify(epoch_authorized_voter)?;

        // The offset in slots `n` on which the target_epoch
        // (default value `DEFAULT_LEADER_SCHEDULE_SLOT_OFFSET`) is
        // calculated is the number of slots available from the
        // first slot `S` of an epoch in which to set a new voter for
        // the epoch at `S` + `n`
        if self.authorized_voters.contains(target_epoch) {
            return Err(VoteError::TooSoonToReauthorize.into());
        }

        // Get the latest authorized_voter
        let (latest_epoch, latest_authorized_pubkey) = self
            .authorized_voters
            .last()
            .ok_or(InstructionError::InvalidAccountData)?;

        // If we're not setting the same pubkey as authorized pubkey again,
        // then update the list of prior voters to mark the expiration
        // of the old authorized pubkey
        if latest_authorized_pubkey != authorized_pubkey {
            // Update the epoch ranges of authorized pubkeys that will be expired
            let epoch_of_last_authorized_switch =
                self.prior_voters.last().map(|range| range.2).unwrap_or(0);

            // target_epoch must:
            // 1) Be monotonically increasing due to the clock always
            //    moving forward
            // 2) not be equal to latest epoch otherwise this
            //    function would have returned TooSoonToReauthorize error
            //    above
            if target_epoch <= *latest_epoch {
                return Err(InstructionError::InvalidAccountData);
            }

            // Commit the new state
            self.prior_voters.append((
                *latest_authorized_pubkey,
                epoch_of_last_authorized_switch,
                target_epoch,
            ));
        }

        self.authorized_voters
            .insert(target_epoch, *authorized_pubkey);

        Ok(())
    }

    fn get_and_update_authorized_voter(
        &mut self,
        current_epoch: Epoch,
    ) -> Result<Pubkey, InstructionError> {
        self.get_and_update_authorized_voter(current_epoch)
    }

    fn commission(&self) -> u8 {
        self.commission
    }

    fn set_commission(&mut self, commission: u8) {
        self.commission = commission;
    }

    fn node_pubkey(&self) -> &Pubkey {
        &self.node_pubkey
    }

    fn set_node_pubkey(&mut self, node_pubkey: Pubkey) {
        self.node_pubkey = node_pubkey;
    }

    fn votes(&self) -> &VecDeque<LandedVote> {
        &self.votes
    }

    fn votes_mut(&mut self) -> &mut VecDeque<LandedVote> {
        &mut self.votes
    }

    fn set_votes(&mut self, votes: VecDeque<LandedVote>) {
        self.votes = votes;
    }

    fn contains_slot(&self, slot: Slot) -> bool {
        self.contains_slot(slot)
    }

    fn last_voted_slot(&self) -> Option<Slot> {
        self.last_voted_slot()
    }

    fn root_slot(&self) -> Option<Slot> {
        self.root_slot
    }

    fn set_root_slot(&mut self, root_slot: Option<Slot>) {
        self.root_slot = root_slot;
    }

    fn current_epoch(&self) -> Epoch {
        self.current_epoch()
    }

    fn epoch_credits_last(&self) -> Option<&(Epoch, u64, u64)> {
        self.epoch_credits.last()
    }

    fn credits_for_vote_at_index(&self, index: usize) -> u64 {
        self.credits_for_vote_at_index(index)
    }

    fn increment_credits(&mut self, epoch: Epoch, credits: u64) {
        self.increment_credits(epoch, credits)
    }

    fn process_timestamp(&mut self, slot: Slot, timestamp: i64) -> Result<(), VoteError> {
        self.process_timestamp(slot, timestamp)
    }

    fn process_next_vote_slot(&mut self, next_vote_slot: Slot, epoch: Epoch, current_slot: Slot) {
        self.process_next_vote_slot(next_vote_slot, epoch, current_slot)
    }

    fn set_vote_account_state(
        self,
        vote_account: &mut BorrowedInstructionAccount,
    ) -> Result<(), InstructionError> {
        // If the account is not large enough to store the vote state, then attempt a realloc to make it large enough.
        // The realloc can only proceed if the vote account has balance sufficient for rent exemption at the new size.
        if (vote_account.get_data().len() < VoteStateV3::size_of())
            && (!vote_account.is_rent_exempt_at_data_length(VoteStateV3::size_of())
                || vote_account
                    .set_data_length(VoteStateV3::size_of())
                    .is_err())
        {
            // Account cannot be resized to the size of a vote state as it will not be rent exempt, or failed to be
            // resized for other reasons.  So store the V1_14_11 version.
            return vote_account.set_state(&VoteStateVersions::V1_14_11(Box::new(
                VoteState1_14_11::from(self),
            )));
        }
        // Vote account is large enough to store the newest version of vote state
        vote_account.set_state(&VoteStateVersions::V3(Box::new(self)))
    }
}

/// The target version to convert all deserialized vote state into.
pub enum VoteStateTargetVersion {
    V3,
    // New vote state versions will be added here...
}

#[derive(Clone, Debug, PartialEq)]
enum TargetVoteState {
    V3(VoteStateV3),
    // New vote state versions will be added here...
}

/// Vote state handler for
/// * Deserializing vote state
/// * Converting vote state in-memory to target version
/// * Operating on the vote state data agnostically
/// * Serializing the resulting state to the vote account
#[derive(Clone, Debug, PartialEq)]
pub struct VoteStateHandler {
    target_state: TargetVoteState,
}

impl VoteStateHandle for VoteStateHandler {
    fn is_uninitialized(&self) -> bool {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.is_uninitialized(),
        }
    }

    fn authorized_withdrawer(&self) -> &Pubkey {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.authorized_withdrawer(),
        }
    }

    fn set_authorized_withdrawer(&mut self, authorized_withdrawer: Pubkey) {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => v3.set_authorized_withdrawer(authorized_withdrawer),
        }
    }

    fn set_new_authorized_voter<F>(
        &mut self,
        authorized_pubkey: &Pubkey,
        current_epoch: Epoch,
        target_epoch: Epoch,
        verify: F,
    ) -> Result<(), InstructionError>
    where
        F: Fn(Pubkey) -> Result<(), InstructionError>,
    {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => {
                v3.set_new_authorized_voter(authorized_pubkey, current_epoch, target_epoch, verify)
            }
        }
    }

    fn get_and_update_authorized_voter(
        &mut self,
        current_epoch: Epoch,
    ) -> Result<Pubkey, InstructionError> {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => v3.get_and_update_authorized_voter(current_epoch),
        }
    }

    fn commission(&self) -> u8 {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.commission(),
        }
    }

    fn set_commission(&mut self, commission: u8) {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => v3.set_commission(commission),
        }
    }

    fn node_pubkey(&self) -> &Pubkey {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.node_pubkey(),
        }
    }

    fn set_node_pubkey(&mut self, node_pubkey: Pubkey) {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => v3.set_node_pubkey(node_pubkey),
        }
    }

    fn votes(&self) -> &VecDeque<LandedVote> {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.votes(),
        }
    }

    fn votes_mut(&mut self) -> &mut VecDeque<LandedVote> {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => v3.votes_mut(),
        }
    }

    fn set_votes(&mut self, votes: VecDeque<LandedVote>) {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => v3.set_votes(votes),
        }
    }

    fn contains_slot(&self, slot: Slot) -> bool {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.contains_slot(slot),
        }
    }

    fn last_voted_slot(&self) -> Option<Slot> {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.last_voted_slot(),
        }
    }

    fn root_slot(&self) -> Option<Slot> {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.root_slot(),
        }
    }

    fn set_root_slot(&mut self, root_slot: Option<Slot>) {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => v3.set_root_slot(root_slot),
        }
    }

    fn current_epoch(&self) -> Epoch {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.current_epoch(),
        }
    }

    fn epoch_credits_last(&self) -> Option<&(Epoch, u64, u64)> {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.epoch_credits_last(),
        }
    }

    fn credits_for_vote_at_index(&self, index: usize) -> u64 {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.credits_for_vote_at_index(index),
        }
    }

    fn increment_credits(&mut self, epoch: Epoch, credits: u64) {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => v3.increment_credits(epoch, credits),
        }
    }

    fn process_timestamp(&mut self, slot: Slot, timestamp: i64) -> Result<(), VoteError> {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => v3.process_timestamp(slot, timestamp),
        }
    }

    fn process_next_vote_slot(&mut self, next_vote_slot: Slot, epoch: Epoch, current_slot: Slot) {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => {
                v3.process_next_vote_slot(next_vote_slot, epoch, current_slot)
            }
        }
    }

    fn set_vote_account_state(
        self,
        vote_account: &mut BorrowedInstructionAccount,
    ) -> Result<(), InstructionError> {
        match self.target_state {
            TargetVoteState::V3(v3) => v3.set_vote_account_state(vote_account),
        }
    }
}

impl VoteStateHandler {
    /// Create a new handler for the provided target version by deserializing
    /// the vote state and converting it to the target.
    pub fn deserialize_and_convert(
        vote_account: &BorrowedInstructionAccount,
        target_version: VoteStateTargetVersion,
    ) -> Result<Self, InstructionError> {
        let target_state = match target_version {
            VoteStateTargetVersion::V3 => {
                let vote_state = VoteStateV3::deserialize(vote_account.get_data())?;
                TargetVoteState::V3(vote_state)
            }
        };
        Ok(Self { target_state })
    }

    pub fn init_vote_account_state(
        vote_account: &mut BorrowedInstructionAccount,
        vote_init: &VoteInit,
        clock: &Clock,
        target_version: VoteStateTargetVersion,
    ) -> Result<(), InstructionError> {
        let state = match target_version {
            VoteStateTargetVersion::V3 => {
                VoteStateVersions::V3(Box::new(VoteStateV3::new(vote_init, clock)))
            }
        };
        vote_account.set_state(&state)
    }

    pub fn deinitialize_vote_account_state(
        vote_account: &mut BorrowedInstructionAccount,
        target_version: VoteStateTargetVersion,
    ) -> Result<(), InstructionError> {
        let state = match target_version {
            VoteStateTargetVersion::V3 => VoteStateVersions::V3(Box::<VoteStateV3>::default()),
        };
        vote_account.set_state(&state)
    }

    pub fn check_vote_account_length(
        vote_account: &mut BorrowedInstructionAccount,
        target_version: VoteStateTargetVersion,
    ) -> Result<(), InstructionError> {
        let length = vote_account.get_data().len();
        let expected = match target_version {
            VoteStateTargetVersion::V3 => VoteStateV3::size_of(),
        };
        if length != expected {
            Err(InstructionError::InvalidAccountData)
        } else {
            Ok(())
        }
    }

    #[cfg(test)]
    pub fn new_v3(vote_state: VoteStateV3) -> Self {
        Self {
            target_state: TargetVoteState::V3(vote_state),
        }
    }

    #[cfg(test)]
    pub fn default_v3() -> Self {
        Self::new_v3(VoteStateV3::default())
    }

    #[cfg(test)]
    pub fn last_lockout(&self) -> Option<&Lockout> {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.last_lockout(),
        }
    }

    #[cfg(test)]
    pub fn credits(&self) -> u64 {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.credits(),
        }
    }

    #[cfg(test)]
    pub fn epoch_credits(&self) -> &Vec<(Epoch, u64, u64)> {
        match &self.target_state {
            TargetVoteState::V3(v3) => &v3.epoch_credits,
        }
    }

    #[cfg(test)]
    pub fn nth_recent_lockout(&self, position: usize) -> Option<&Lockout> {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.nth_recent_lockout(position),
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_clock::Clock,
        solana_epoch_schedule::MAX_LEADER_SCHEDULE_EPOCH_OFFSET,
        solana_pubkey::Pubkey,
        solana_vote_interface::{
            authorized_voters::AuthorizedVoters,
            state::{VoteInit, MAX_EPOCH_CREDITS_HISTORY, MAX_LOCKOUT_HISTORY},
        },
    };

    fn get_max_sized_vote_state() -> VoteStateV3 {
        let mut authorized_voters = AuthorizedVoters::default();
        for i in 0..=MAX_LEADER_SCHEDULE_EPOCH_OFFSET {
            authorized_voters.insert(i, Pubkey::new_unique());
        }

        VoteStateV3 {
            votes: VecDeque::from(vec![LandedVote::default(); MAX_LOCKOUT_HISTORY]),
            root_slot: Some(u64::MAX),
            epoch_credits: vec![(0, 0, 0); MAX_EPOCH_CREDITS_HISTORY],
            authorized_voters,
            ..Default::default()
        }
    }

    #[test]
    fn test_set_new_authorized_voter() {
        let original_voter = Pubkey::new_unique();
        let epoch_offset = 15;
        let mut vote_state = VoteStateV3::new(
            &VoteInit {
                node_pubkey: original_voter,
                authorized_voter: original_voter,
                authorized_withdrawer: original_voter,
                commission: 0,
            },
            &Clock::default(),
        );

        assert!(vote_state.prior_voters.last().is_none());

        let new_voter = Pubkey::new_unique();
        // Set a new authorized voter
        vote_state
            .set_new_authorized_voter(&new_voter, 0, epoch_offset, |_| Ok(()))
            .unwrap();

        // assert_eq!(vote_state.prior_voters.idx, 0); TODO: Field `idx` is private.
        assert_eq!(
            vote_state.prior_voters.last(),
            Some(&(original_voter, 0, epoch_offset))
        );

        // Trying to set authorized voter for same epoch again should fail
        assert_eq!(
            vote_state.set_new_authorized_voter(&new_voter, 0, epoch_offset, |_| Ok(())),
            Err(VoteError::TooSoonToReauthorize.into())
        );

        // Setting the same authorized voter again should succeed
        vote_state
            .set_new_authorized_voter(&new_voter, 2, 2 + epoch_offset, |_| Ok(()))
            .unwrap();

        // Set a third and fourth authorized voter
        let new_voter2 = Pubkey::new_unique();
        vote_state
            .set_new_authorized_voter(&new_voter2, 3, 3 + epoch_offset, |_| Ok(()))
            .unwrap();
        // assert_eq!(vote_state.prior_voters.idx, 1); TODO: Field `idx` is private.
        assert_eq!(
            vote_state.prior_voters.last(),
            Some(&(new_voter, epoch_offset, 3 + epoch_offset))
        );

        let new_voter3 = Pubkey::new_unique();
        vote_state
            .set_new_authorized_voter(&new_voter3, 6, 6 + epoch_offset, |_| Ok(()))
            .unwrap();
        // assert_eq!(vote_state.prior_voters.idx, 2); TODO: Field `idx` is private.
        assert_eq!(
            vote_state.prior_voters.last(),
            Some(&(new_voter2, 3 + epoch_offset, 6 + epoch_offset))
        );

        // Check can set back to original voter
        vote_state
            .set_new_authorized_voter(&original_voter, 9, 9 + epoch_offset, |_| Ok(()))
            .unwrap();

        // Run with these voters for a while, check the ranges of authorized
        // voters is correct
        for i in 9..epoch_offset {
            assert_eq!(
                vote_state.get_and_update_authorized_voter(i).unwrap(),
                original_voter
            );
        }
        for i in epoch_offset..3 + epoch_offset {
            assert_eq!(
                vote_state.get_and_update_authorized_voter(i).unwrap(),
                new_voter
            );
        }
        for i in 3 + epoch_offset..6 + epoch_offset {
            assert_eq!(
                vote_state.get_and_update_authorized_voter(i).unwrap(),
                new_voter2
            );
        }
        for i in 6 + epoch_offset..9 + epoch_offset {
            assert_eq!(
                vote_state.get_and_update_authorized_voter(i).unwrap(),
                new_voter3
            );
        }
        for i in 9 + epoch_offset..=10 + epoch_offset {
            assert_eq!(
                vote_state.get_and_update_authorized_voter(i).unwrap(),
                original_voter
            );
        }
    }

    #[test]
    fn test_authorized_voter_is_locked_within_epoch() {
        let original_voter = Pubkey::new_unique();
        let mut vote_state = VoteStateV3::new(
            &VoteInit {
                node_pubkey: original_voter,
                authorized_voter: original_voter,
                authorized_withdrawer: original_voter,
                commission: 0,
            },
            &Clock::default(),
        );
        // Test that it's not possible to set a new authorized
        // voter within the same epoch, even if none has been
        // explicitly set before
        let new_voter = Pubkey::new_unique();
        assert_eq!(
            vote_state.set_new_authorized_voter(&new_voter, 1, 1, |_| Ok(())),
            Err(VoteError::TooSoonToReauthorize.into())
        );
        assert_eq!(
            vote_state.authorized_voters.get_authorized_voter(1),
            Some(original_voter)
        );
        // Set a new authorized voter for a future epoch
        assert_eq!(
            vote_state.set_new_authorized_voter(&new_voter, 1, 2, |_| Ok(())),
            Ok(())
        );
        // Test that it's not possible to set a new authorized
        // voter within the same epoch, even if none has been
        // explicitly set before
        assert_eq!(
            vote_state.set_new_authorized_voter(&original_voter, 3, 3, |_| Ok(())),
            Err(VoteError::TooSoonToReauthorize.into())
        );
        assert_eq!(
            vote_state.authorized_voters.get_authorized_voter(3),
            Some(new_voter)
        );
    }

    #[test]
    fn test_vote_state_max_size() {
        let mut max_sized_data = vec![0; VoteStateV3::size_of()];
        let vote_state = get_max_sized_vote_state();
        let (start_leader_schedule_epoch, _) = vote_state.authorized_voters.last().unwrap();
        let start_current_epoch =
            start_leader_schedule_epoch - MAX_LEADER_SCHEDULE_EPOCH_OFFSET + 1;

        let mut vote_state = Some(vote_state);
        for i in start_current_epoch..start_current_epoch + 2 * MAX_LEADER_SCHEDULE_EPOCH_OFFSET {
            vote_state.as_mut().map(|vote_state| {
                vote_state.set_new_authorized_voter(
                    &Pubkey::new_unique(),
                    i,
                    i + MAX_LEADER_SCHEDULE_EPOCH_OFFSET,
                    |_| Ok(()),
                )
            });

            let versioned = VoteStateVersions::new_v3(vote_state.take().unwrap());
            VoteStateV3::serialize(&versioned, &mut max_sized_data).unwrap();
            vote_state = match versioned {
                VoteStateVersions::V3(v3) => Some(*v3),
                _ => panic!("should be v3"),
            };
        }
    }
}
