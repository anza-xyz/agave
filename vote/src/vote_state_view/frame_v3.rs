use {
    super::{
        field_frames::{
            AuthorizedVotersListFrame, EpochCreditsListFrame, ListFrame, PriorVotersFrame,
            RootSlotFrame, VotesListFrame,
        },
        Field, Result, VoteStateViewError,
    },
    solana_pubkey::Pubkey,
    solana_vote_interface::state::BlockTimestamp,
    std::io::BufRead,
};

#[derive(Debug, Clone)]
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
pub(super) struct VoteStateFrameV3 {
    pub(super) num_votes: u8,
    pub(super) has_root_slot: bool,
    pub(super) num_authorized_voters: u8,
    pub(super) num_epoch_credits: u8,
}

impl VoteStateFrameV3 {
    pub(super) fn try_new(bytes: &[u8]) -> Result<Self> {
        let votes_offset = Self::votes_offset();
        let mut cursor = std::io::Cursor::new(bytes);
        cursor.set_position(votes_offset as u64);

        let votes = VotesListFrame::read(&mut cursor, true /* has_latency */)?;
        let root_slot = RootSlotFrame::read(&mut cursor)?;
        let authorized_voters = AuthorizedVotersListFrame::read(&mut cursor)?;
        PriorVotersFrame::read(&mut cursor);
        let epoch_credits = EpochCreditsListFrame::read(&mut cursor)?;
        cursor.consume(core::mem::size_of::<BlockTimestamp>());
        if cursor.position() as usize <= bytes.len() {
            Ok(Self {
                num_votes: u8::try_from(votes.len()).map_err(|_| VoteStateViewError::ParseError)?,
                has_root_slot: root_slot.has_root_slot(),
                num_authorized_voters: u8::try_from(authorized_voters.len())
                    .map_err(|_| VoteStateViewError::ParseError)?,
                num_epoch_credits: u8::try_from(epoch_credits.len())
                    .map_err(|_| VoteStateViewError::ParseError)?,
            })
        } else {
            Err(VoteStateViewError::ParseError)
        }
    }

    pub(super) fn votes_frame(&self) -> VotesListFrame {
        VotesListFrame::new(self.num_votes as usize, true /* has_latency */)
    }

    pub(super) fn root_slot_frame(&self) -> RootSlotFrame {
        RootSlotFrame::new(self.has_root_slot)
    }

    pub(super) fn authorized_voters_frame(&self) -> AuthorizedVotersListFrame {
        AuthorizedVotersListFrame::new(self.num_authorized_voters as usize)
    }

    pub(super) const fn epoch_credits_frame(&self) -> EpochCreditsListFrame {
        EpochCreditsListFrame::new(self.num_epoch_credits as usize)
    }

    pub(super) fn get_field_offset(&self, field: Field) -> usize {
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
        4 // size of version
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
        Self::votes_offset() + self.votes_frame().total_size()
    }

    fn authorized_voters_offset(&self) -> usize {
        self.root_slot_offset() + self.root_slot_frame().total_size()
    }

    fn prior_voters_offset(&self) -> usize {
        self.authorized_voters_offset() + self.authorized_voters_frame().total_size()
    }

    fn epoch_credits_offset(&self) -> usize {
        self.prior_voters_offset() + PriorVotersFrame::total_size()
    }

    fn last_timestamp_offset(&self) -> usize {
        self.epoch_credits_offset() + self.epoch_credits_frame().total_size()
    }
}
