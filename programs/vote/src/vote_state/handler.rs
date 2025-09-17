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
        self.is_uninitialized()
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
        self.set_new_authorized_voter(authorized_pubkey, current_epoch, target_epoch, verify)
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
