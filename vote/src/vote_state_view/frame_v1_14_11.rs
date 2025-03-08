use {
    super::{
        field_frames::{
            AuthorizedVotersListFrame, ListFrame, LockoutListFrame, PriorVotersFrame, RootSlotFrame,
        },
        EpochCreditsListFrame, Field, Result, VoteStateViewError,
    },
    solana_pubkey::Pubkey,
    solana_vote_interface::state::BlockTimestamp,
    std::io::BufRead,
};

#[derive(Debug, Clone)]
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
            Err(VoteStateViewError::ParseError)
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
        self.prior_voters_offset() + PriorVotersFrame.total_size()
    }

    fn last_timestamp_offset(&self) -> usize {
        self.epoch_credits_offset() + self.epoch_credits_frame.total_size()
    }
}
