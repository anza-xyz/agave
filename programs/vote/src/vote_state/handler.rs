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

#![allow(unused)]

use {
    solana_clock::{Clock, Epoch, Slot},
    solana_instruction::error::InstructionError,
    solana_pubkey::Pubkey,
    solana_transaction_context::BorrowedInstructionAccount,
    solana_vote_interface::{
        error::VoteError,
        state::{
            LandedVote, Lockout, VoteInit, VoteState1_14_11, VoteStateV3, VoteStateV4,
            VoteStateVersions,
        },
    },
    std::{collections::VecDeque, ops::Deref},
};

/// The target version to convert all deserialized vote state into.
pub enum VoteStateTargetVersion {
    V3,
    V4,
}

enum TargetVoteState {
    V3(VoteStateV3),
    V4(VoteStateV4),
}

/// Vote state handler for
/// * Deserializing vote state
/// * Converting vote state in-memory to target version
/// * Operating on the vote state data agnostically
/// * Serializing the resulting state to the vote account
pub struct VoteStateHandler {
    target_state: TargetVoteState,
}

impl VoteStateHandler {
    /// Create a new handler for the provided target version by deserializing
    /// the vote state and converting it to the target.
    pub fn deserialize_and_convert(
        vote_account: &BorrowedInstructionAccount,
        target_version: VoteStateTargetVersion,
    ) -> Result<Self, InstructionError> {
        let state = vote_account.get_state::<VoteStateVersions>()?;
        let target_state = match target_version {
            VoteStateTargetVersion::V3 => TargetVoteState::V3(state.convert_to_v3()),
            VoteStateTargetVersion::V4 => {
                // Requires vote-interface release or local reimplementation.
                unimplemented!()
            }
        };
        Ok(Self { target_state })
    }

    /// Check if the vote state is uninitialized.
    pub fn is_uninitialized(&self) -> bool {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.node_pubkey == Pubkey::default(),
            TargetVoteState::V4(v4) => v4.node_pubkey == Pubkey::default(),
        }
    }

    pub fn authorized_withdrawer(&self) -> &Pubkey {
        match &self.target_state {
            TargetVoteState::V3(v3) => &v3.authorized_withdrawer,
            TargetVoteState::V4(v4) => &v4.authorized_withdrawer,
        }
    }

    pub fn set_authorized_withdrawer(&mut self, authorized_withdrawer: Pubkey) {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => v3.authorized_withdrawer = authorized_withdrawer,
            TargetVoteState::V4(v4) => v4.authorized_withdrawer = authorized_withdrawer,
        }
    }

    pub fn set_new_authorized_voter<F>(
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
            TargetVoteState::V4(_v4) => {
                // V4 implementation not yet available
                unimplemented!("V4 vote state not yet implemented")
            }
        }
    }

    pub fn get_and_update_authorized_voter(
        &mut self,
        current_epoch: Epoch,
    ) -> Result<Pubkey, InstructionError> {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => v3.get_and_update_authorized_voter(current_epoch),
            TargetVoteState::V4(_v4) => {
                // V4 implementation not yet available
                unimplemented!("V4 vote state not yet implemented")
            }
        }
    }

    pub fn commission(&self) -> u8 {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.commission,
            TargetVoteState::V4(_v4) => {
                // V4 implementation not yet available
                unimplemented!("V4 vote state not yet implemented")
            }
        }
    }

    pub fn set_commission(&mut self, commission: u8) {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => v3.commission = commission,
            TargetVoteState::V4(_v4) => {
                // V4 implementation not yet available
                unimplemented!("V4 vote state not yet implemented")
            }
        }
    }

    pub fn node_pubkey(&self) -> &Pubkey {
        match &self.target_state {
            TargetVoteState::V3(v3) => &v3.node_pubkey,
            TargetVoteState::V4(_v4) => {
                // V4 implementation not yet available
                unimplemented!("V4 vote state not yet implemented")
            }
        }
    }

    pub fn set_node_pubkey(&mut self, node_pubkey: Pubkey) {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => v3.node_pubkey = node_pubkey,
            TargetVoteState::V4(_v4) => {
                // V4 implementation not yet available
                unimplemented!("V4 vote state not yet implemented")
            }
        }
    }

    pub fn votes(&self) -> &VecDeque<LandedVote> {
        match &self.target_state {
            TargetVoteState::V3(v3) => &v3.votes,
            TargetVoteState::V4(_v4) => {
                // V4 implementation not yet available
                unimplemented!("V4 vote state not yet implemented")
            }
        }
    }

    pub fn votes_mut(&mut self) -> &mut VecDeque<LandedVote> {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => &mut v3.votes,
            TargetVoteState::V4(_v4) => {
                // V4 implementation not yet available
                unimplemented!("V4 vote state not yet implemented")
            }
        }
    }

    pub fn set_votes(&mut self, votes: VecDeque<LandedVote>) {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => v3.votes = votes,
            TargetVoteState::V4(_v4) => {
                // V4 implementation not yet available
                unimplemented!("V4 vote state not yet implemented")
            }
        }
    }

    pub fn contains_slot(&self, slot: Slot) -> bool {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.contains_slot(slot),
            TargetVoteState::V4(_v4) => {
                // V4 implementation not yet available
                unimplemented!("V4 vote state not yet implemented")
            }
        }
    }

    pub fn last_voted_slot(&self) -> Option<Slot> {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.last_voted_slot(),
            TargetVoteState::V4(_v4) => {
                // V4 implementation not yet available
                unimplemented!("V4 vote state not yet implemented")
            }
        }
    }

    pub fn compute_vote_latency(voted_for_slot: Slot, current_slot: Slot) -> u8 {
        std::cmp::min(current_slot.saturating_sub(voted_for_slot), u8::MAX as u64) as u8
    }

    pub fn root_slot(&self) -> Option<Slot> {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.root_slot,
            TargetVoteState::V4(_v4) => {
                // V4 implementation not yet available
                unimplemented!("V4 vote state not yet implemented")
            }
        }
    }

    pub fn set_root_slot(&mut self, root_slot: Option<Slot>) {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => v3.root_slot = root_slot,
            TargetVoteState::V4(_v4) => {
                // V4 implementation not yet available
                unimplemented!("V4 vote state not yet implemented")
            }
        }
    }

    pub fn current_epoch(&self) -> Epoch {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.current_epoch(),
            TargetVoteState::V4(_v4) => {
                // V4 implementation not yet available
                unimplemented!("V4 vote state not yet implemented")
            }
        }
    }

    pub fn epoch_credits_last(&self) -> Option<&(Epoch, u64, u64)> {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.epoch_credits.last(),
            TargetVoteState::V4(_v4) => {
                // V4 implementation not yet available
                unimplemented!("V4 vote state not yet implemented")
            }
        }
    }

    pub fn credits_for_vote_at_index(&self, index: usize) -> u64 {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.credits_for_vote_at_index(index),
            TargetVoteState::V4(_v4) => {
                // V4 implementation not yet available
                unimplemented!("V4 vote state not yet implemented")
            }
        }
    }

    pub fn increment_credits(&mut self, epoch: Epoch, credits: u64) {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => v3.increment_credits(epoch, credits),
            TargetVoteState::V4(_v4) => {
                // V4 implementation not yet available
                unimplemented!("V4 vote state not yet implemented")
            }
        }
    }

    pub fn process_timestamp(&mut self, slot: Slot, timestamp: i64) -> Result<(), VoteError> {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => v3.process_timestamp(slot, timestamp),
            TargetVoteState::V4(_v4) => {
                // V4 implementation not yet available
                unimplemented!("V4 vote state not yet implemented")
            }
        }
    }

    pub fn process_next_vote_slot(
        &mut self,
        next_vote_slot: Slot,
        epoch: Epoch,
        current_slot: Slot,
    ) {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => {
                v3.process_next_vote_slot(next_vote_slot, epoch, current_slot)
            }
            TargetVoteState::V4(_v4) => {
                // V4 implementation not yet available
                unimplemented!("V4 vote state not yet implemented")
            }
        }
    }

    pub fn deinitialize(&mut self) {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => *v3 = VoteStateV3::default(),
            TargetVoteState::V4(v4) => *v4 = VoteStateV4::default(),
        }
    }

    pub fn set_vote_account_state(
        &self,
        vote_account: &mut BorrowedInstructionAccount,
    ) -> Result<(), InstructionError> {
        match &self.target_state {
            TargetVoteState::V3(v3) => {
                // If the account is not large enough to store the vote state, then attempt a realloc to make it large enough.
                // The realloc can only proceed if the vote account has balance sufficient for rent exemption at the new size.
                if (vote_account.get_data().len() < VoteStateVersions::vote_state_size_of(true))
                    && (!vote_account
                        .is_rent_exempt_at_data_length(VoteStateVersions::vote_state_size_of(true))
                        || vote_account
                            .set_data_length(VoteStateVersions::vote_state_size_of(true))
                            .is_err())
                {
                    // Account cannot be resized to the size of a vote state as it will not be rent exempt, or failed to be
                    // resized for other reasons.  So store the V1_14_11 version.
                    return vote_account.set_state(&VoteStateVersions::V1_14_11(Box::new(
                        VoteState1_14_11::from(v3.clone()),
                    )));
                }
                // Vote account is large enough to store the newest version of vote state
                vote_account.set_state(&VoteStateVersions::V3(Box::new(v3.clone())))
            }
            TargetVoteState::V4(_v4) => {
                // V4 implementation not yet available
                unimplemented!("V4 vote state not yet implemented")
            }
        }
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
            VoteStateTargetVersion::V4 => {
                // VoteStateVersions::V4(Box::new(VoteStateV4::new(vote_init, clock)))
                unimplemented!()
            }
        };
        vote_account.set_state(&state)
    }
}
