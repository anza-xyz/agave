use {
    super::{
        EpochCreditsListFrame, Field, Result, VoteStateViewError,
        field_frames::{
            AuthorizedVotersListFrame, ListFrame, LockoutListFrame, PriorVotersFrame, RootSlotFrame,
        },
    },
    solana_pubkey::Pubkey,
    solana_vote_interface::state::BlockTimestamp,
    std::io::BufRead,
};

#[derive(Debug, PartialEq, Clone, Copy)]
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
pub(super) struct VoteStateFrameV1_14_11 {
    pub(super) votes_frame: LockoutListFrame,
    pub(super) root_slot_frame: RootSlotFrame,
    pub(super) authorized_voters_frame: AuthorizedVotersListFrame,
    pub(super) epoch_credits_frame: EpochCreditsListFrame,
}

impl VoteStateFrameV1_14_11 {
    pub(super) fn try_new(bytes: &[u8]) -> Result<Self> {
        let votes_offset = Self::votes_offset();
        let mut cursor = std::io::Cursor::new(bytes);
        cursor.set_position(votes_offset as u64);

        let votes_frame = LockoutListFrame::read(&mut cursor)?;
        let root_slot_frame = RootSlotFrame::read(&mut cursor)?;
        let authorized_voters_frame = AuthorizedVotersListFrame::read(&mut cursor)?;
        PriorVotersFrame::read(&mut cursor);
        let epoch_credits_frame = EpochCreditsListFrame::read(&mut cursor)?;
        cursor.consume(core::mem::size_of::<BlockTimestamp>());
        // trailing bytes are allowed. consistent with default behavior of
        // function bincode::deserialize
        if cursor.position() as usize <= bytes.len() {
            Ok(Self {
                votes_frame,
                root_slot_frame,
                authorized_voters_frame,
                epoch_credits_frame,
            })
        } else {
            Err(VoteStateViewError::AccountDataTooSmall)
        }
    }

    pub(super) fn field_offset(&self, field: Field) -> usize {
        match field {
            Field::NodePubkey => Self::node_pubkey_offset(),
            Field::Commission => Self::commission_offset(),
            Field::Votes => Self::votes_offset(),
            Field::RootSlot => self.root_slot_offset(),
            Field::AuthorizedVoters => self.authorized_voters_offset(),
            Field::EpochCredits => self.epoch_credits_offset(),
            Field::LastTimestamp => self.last_timestamp_offset(),
        }
    }

    const fn node_pubkey_offset() -> usize {
        core::mem::size_of::<u32>() // version
    }

    const fn authorized_withdrawer_offset() -> usize {
        Self::node_pubkey_offset() + core::mem::size_of::<Pubkey>()
    }

    const fn commission_offset() -> usize {
        Self::authorized_withdrawer_offset() + core::mem::size_of::<Pubkey>()
    }

    const fn votes_offset() -> usize {
        Self::commission_offset() + core::mem::size_of::<u8>()
    }

    fn root_slot_offset(&self) -> usize {
        Self::votes_offset() + self.votes_frame.total_size()
    }

    fn authorized_voters_offset(&self) -> usize {
        self.root_slot_offset() + self.root_slot_frame.total_size()
    }

    fn prior_voters_offset(&self) -> usize {
        self.authorized_voters_offset() + self.authorized_voters_frame.total_size()
    }

    fn epoch_credits_offset(&self) -> usize {
        self.prior_voters_offset() + PriorVotersFrame::total_size()
    }

    fn last_timestamp_offset(&self) -> usize {
        self.epoch_credits_offset() + self.epoch_credits_frame.total_size()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_clock::Clock,
        solana_vote_interface::state::{
            LandedVote, Lockout, VoteInit, VoteState1_14_11, VoteStateV3, VoteStateVersions,
        },
    };

    #[test]
    fn test_try_new_zeroed() {
        let target_vote_state = VoteState1_14_11::default();
        let target_vote_state_versions = VoteStateVersions::V1_14_11(Box::new(target_vote_state));
        let mut bytes = bincode::serialize(&target_vote_state_versions).unwrap();

        for i in 0..bytes.len() {
            let vote_state_frame = VoteStateFrameV1_14_11::try_new(&bytes[..i]);
            assert_eq!(
                vote_state_frame,
                Err(VoteStateViewError::AccountDataTooSmall)
            );
        }

        for has_trailing_bytes in [false, true] {
            if has_trailing_bytes {
                bytes.extend_from_slice(&[0; 42]);
            }
            assert_eq!(
                VoteStateFrameV1_14_11::try_new(&bytes),
                Ok(VoteStateFrameV1_14_11 {
                    votes_frame: LockoutListFrame { len: 0 },
                    root_slot_frame: RootSlotFrame {
                        has_root_slot: false,
                    },
                    authorized_voters_frame: AuthorizedVotersListFrame { len: 0 },
                    epoch_credits_frame: EpochCreditsListFrame { len: 0 },
                })
            );
        }
    }

    #[test]
    fn test_try_new_simple() {
        let mut target_vote_state = VoteStateV3::new(&VoteInit::default(), &Clock::default());
        target_vote_state.root_slot = Some(42);
        target_vote_state.epoch_credits.push((1, 2, 3));
        target_vote_state.votes.push_back(LandedVote {
            latency: 0,
            lockout: Lockout::default(),
        });

        let target_vote_state_versions =
            VoteStateVersions::V1_14_11(Box::new(target_vote_state.into()));
        let mut bytes = bincode::serialize(&target_vote_state_versions).unwrap();

        for i in 0..bytes.len() {
            let vote_state_frame = VoteStateFrameV1_14_11::try_new(&bytes[..i]);
            assert_eq!(
                vote_state_frame,
                Err(VoteStateViewError::AccountDataTooSmall)
            );
        }

        for has_trailing_bytes in [false, true] {
            if has_trailing_bytes {
                bytes.extend_from_slice(&[0; 42]);
            }
            assert_eq!(
                VoteStateFrameV1_14_11::try_new(&bytes),
                Ok(VoteStateFrameV1_14_11 {
                    votes_frame: LockoutListFrame { len: 1 },
                    root_slot_frame: RootSlotFrame {
                        has_root_slot: true,
                    },
                    authorized_voters_frame: AuthorizedVotersListFrame { len: 1 },
                    epoch_credits_frame: EpochCreditsListFrame { len: 1 },
                })
            );
        }
    }

    #[test]
    fn test_try_new_invalid_values() {
        let mut bytes = vec![0; VoteStateFrameV1_14_11::votes_offset()];

        {
            let mut bytes = bytes.clone();
            bytes.extend_from_slice(&(256u64.to_le_bytes()));
            let vote_state_frame = VoteStateFrameV1_14_11::try_new(&bytes);
            assert_eq!(
                vote_state_frame,
                Err(VoteStateViewError::InvalidVotesLength)
            );
        }

        bytes.extend_from_slice(&[0; core::mem::size_of::<u64>()]);

        {
            let mut bytes = bytes.clone();
            bytes.extend_from_slice(&(2u8.to_le_bytes()));
            let vote_state_frame = VoteStateFrameV1_14_11::try_new(&bytes);
            assert_eq!(
                vote_state_frame,
                Err(VoteStateViewError::InvalidRootSlotOption)
            );
        }

        bytes.extend_from_slice(&[0; 1]);

        {
            let mut bytes = bytes.clone();
            bytes.extend_from_slice(&(256u64.to_le_bytes()));
            let vote_state_frame = VoteStateFrameV1_14_11::try_new(&bytes);
            assert_eq!(
                vote_state_frame,
                Err(VoteStateViewError::InvalidAuthorizedVotersLength)
            );
        }

        bytes.extend_from_slice(&[0; core::mem::size_of::<u64>()]);
        bytes.extend_from_slice(&[0; PriorVotersFrame::total_size()]);

        {
            let mut bytes = bytes.clone();
            bytes.extend_from_slice(&(256u64.to_le_bytes()));
            let vote_state_frame = VoteStateFrameV1_14_11::try_new(&bytes);
            assert_eq!(
                vote_state_frame,
                Err(VoteStateViewError::InvalidEpochCreditsLength)
            );
        }
    }
}
