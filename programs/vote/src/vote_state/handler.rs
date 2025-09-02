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
    solana_vote_interface::state::{
        VoteInit, VoteState1_14_11, VoteStateV3, VoteStateV4, VoteStateVersions,
    },
    std::ops::Deref,
};

/// The target version to convert all deserialized vote state into.
pub enum VoteStateTargetVersion {
    V3,
    V4,
}

/// Vote state handler for
/// * Deserializing vote state
/// * Converting vote state in-memory to target version
/// * Operating on the vote state data agnostically
/// * Serializing the resulting state to the vote account
pub struct VoteStateHandler {
    state: VoteStateVersions,
    target_version: VoteStateTargetVersion,
}

impl VoteStateHandler {
    /// Create a new handler for the provided target version by deserializing
    /// the vote state and converting it to the target.
    pub fn deserialize_and_convert(
        vote_account: &BorrowedInstructionAccount,
        target_version: VoteStateTargetVersion,
    ) -> Result<Self, InstructionError> {
        let state = vote_account.get_state::<VoteStateVersions>().map(|state| {
            match target_version {
                VoteStateTargetVersion::V3 => {
                    VoteStateVersions::V3(Box::new(state.convert_to_v3()))
                }
                VoteStateTargetVersion::V4 => {
                    // Requires vote-interface release or local reimplementation.
                    unimplemented!()
                }
            }
        })?;
        Ok(Self {
            state,
            target_version,
        })
    }

    /// Check if the vote state is uninitialized.
    pub fn is_uninitialized(&self) -> bool {
        self.state.is_uninitialized()
    }

    pub fn authorized_withdrawer(&self) -> &Pubkey {
        match self.state {
            VoteStateVersions::V3(ref v3) => &v3.authorized_withdrawer,
            // VoteStateVersions::V4(ref v4) => &v4.authorized_withdrawer,
            _ => panic!("Version not supported for update"),
        }
    }

    pub fn set_authorized_withdrawer(&mut self, authorized_withdrawer: Pubkey) {
        match self.state {
            VoteStateVersions::V3(ref mut v3) => v3.authorized_withdrawer = authorized_withdrawer,
            // VoteStateVersions::V4(ref mut v4) => v4.authorized_withdrawer = authorized_withdrawer,
            _ => panic!("Version not supported for update"),
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
        match self.state {
            VoteStateVersions::V3(ref mut v3) => {
                v3.set_new_authorized_voter(authorized_pubkey, current_epoch, target_epoch, verify)
            }
            // VoteStateVersions::V4(ref mut v4) => {
            //     v4.set_new_authorized_voter(authorized_pubkey, current_epoch, target_epoch, verify)
            // }
            _ => panic!("Version not supported for update"),
        }
    }

    pub fn commission(&self) -> u8 {
        match self.state {
            VoteStateVersions::V3(ref v3) => v3.commission,
            // VoteStateVersions::V4(ref v4) => {
            //     // For v4, return the inflation rewards commission converted back to u8
            //     (v4.inflation_rewards_commission_bps / 100) as u8
            // }
            _ => panic!("Version not supported for update"),
        }
    }

    pub fn set_commission(&mut self, commission: u8) {
        match self.state {
            VoteStateVersions::V3(ref mut v3) => v3.commission = commission,
            // VoteStateVersions::V4(ref mut v4) => {
            //     // For v4, set the inflation rewards commission in basis points
            //     v4.inflation_rewards_commission_bps = u16::from(commission).saturating_mul(100);
            // }
            _ => panic!("Version not supported for update"),
        }
    }

    pub fn node_pubkey(&self) -> &Pubkey {
        match self.state {
            VoteStateVersions::V3(ref v3) => &v3.node_pubkey,
            // VoteStateVersions::V4(ref v4) => &v4.node_pubkey,
            _ => panic!("Version not supported for update"),
        }
    }

    pub fn set_node_pubkey(&mut self, node_pubkey: Pubkey) {
        match self.state {
            VoteStateVersions::V3(ref mut v3) => v3.node_pubkey = node_pubkey,
            // VoteStateVersions::V4(ref mut v4) => v4.node_pubkey = node_pubkey,
            _ => panic!("Version not supported for update"),
        }
    }

    pub fn epoch_credits_last(&self) -> Option<&(Epoch, u64, u64)> {
        match self.state {
            VoteStateVersions::V3(ref v3) => v3.epoch_credits.last(),
            // VoteStateVersions::V4(ref v4) => v4.epoch_credits.last(),
            _ => panic!("Version not supported for update"),
        }
    }

    // TODO: V4 state feature also disallows insufficient lamports.
    pub fn set_vote_account_state(
        &self,
        vote_account: &mut BorrowedInstructionAccount,
    ) -> Result<(), InstructionError> {
        // If the account is not large enough to store the vote state, then attempt a realloc to make it large enough.
        // The realloc can only proceed if the vote account has balance sufficient for rent exemption at the new size.
        if (vote_account.get_data().len() < VoteStateVersions::vote_state_size_of(true))
            && (!vote_account
                .is_rent_exempt_at_data_length(VoteStateVersions::vote_state_size_of(true))
                || vote_account
                    .set_data_length(VoteStateVersions::vote_state_size_of(true))
                    .is_err())
        {
            match &self.state {
                VoteStateVersions::V3(v3) => {
                    // Account cannot be resized to the size of a vote state as it will not be rent exempt, or failed to be
                    // resized for other reasons.  So store the V1_14_11 version.
                    return vote_account.set_state(&VoteStateVersions::V1_14_11(Box::new(
                        VoteState1_14_11::from(v3.deref().to_owned()),
                    )));
                }
                _ => panic!("Version not supported for V1_14_11 conversion"),
            };
        }
        // Vote account is large enough to store the newest version of vote state
        vote_account.set_state(&self.state)
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
