use {
    self::{
        field_frames::{
            AuthorizedVotersListFrame, EpochCreditsItem, EpochCreditsListFrame, RootSlotFrame,
            RootSlotView, VotesFrame,
        },
        frame_v1_14_11::VoteStateFrameV1_14_11,
        frame_v3::VoteStateFrameV3,
        list_view::ListView,
    },
    core::fmt::Debug,
    solana_clock::{Epoch, Slot},
    solana_pubkey::Pubkey,
    solana_vote_interface::state::{BlockTimestamp, Lockout},
    std::sync::Arc,
};
#[cfg(feature = "dev-context-only-utils")]
use {
    bincode,
    solana_vote_interface::state::{VoteState, VoteStateVersions},
};

mod field_frames;
mod frame_v1_14_11;
mod frame_v3;
mod list_view;

#[derive(Debug, PartialEq, Eq)]
pub enum VoteStateViewError {
    AccountDataTooSmall,
    InvalidVotesLength,
    InvalidRootSlotOption,
    InvalidAuthorizedVotersLength,
    InvalidEpochCreditsLength,
    OldVersion,
    UnsupportedVersion,
}

pub type Result<T> = core::result::Result<T, VoteStateViewError>;

enum Field {
    NodePubkey,
    Commission,
    Votes,
    RootSlot,
    AuthorizedVoters,
    EpochCredits,
    LastTimestamp,
}

/// A view into a serialized VoteState.
///
/// This struct provides access to the VoteState data without
/// deserializing it. This is done by parsing and caching metadata
/// about the layout of the serialized VoteState.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
pub struct VoteStateView {
    data: Arc<Vec<u8>>,
    frame: VoteStateFrame,
}

impl VoteStateView {
    pub fn try_new(data: Arc<Vec<u8>>) -> Result<Self> {
        let frame = VoteStateFrame::try_new(data.as_ref())?;
        Ok(Self { data, frame })
    }

    pub fn node_pubkey(&self) -> &Pubkey {
        let offset = self.frame.offset(Field::NodePubkey);
        // SAFETY: `frame` was created from `data`.
        unsafe { &*(self.data.as_ptr().add(offset) as *const Pubkey) }
    }

    pub fn commission(&self) -> u8 {
        let offset = self.frame.offset(Field::Commission);
        // SAFETY: `frame` was created from `data`.
        self.data[offset]
    }

    pub fn votes_iter(&self) -> impl Iterator<Item = Lockout> + '_ {
        self.votes_view().into_iter().map(|vote| {
            Lockout::new_with_confirmation_count(vote.slot(), vote.confirmation_count())
        })
    }

    pub fn last_lockout(&self) -> Option<Lockout> {
        self.votes_view().last().map(|item| {
            Lockout::new_with_confirmation_count(item.slot(), item.confirmation_count())
        })
    }

    pub fn last_voted_slot(&self) -> Option<Slot> {
        self.votes_view().last().map(|item| item.slot())
    }

    pub fn root_slot(&self) -> Option<Slot> {
        self.root_slot_view().root_slot()
    }

    pub fn get_authorized_voter(&self, epoch: Epoch) -> Option<&Pubkey> {
        self.authorized_voters_view().get_authorized_voter(epoch)
    }

    pub fn num_epoch_credits(&self) -> usize {
        self.epoch_credits_view().len()
    }

    pub fn epoch_credits_iter(&self) -> impl Iterator<Item = &EpochCreditsItem> + '_ {
        self.epoch_credits_view().into_iter()
    }

    pub fn credits(&self) -> u64 {
        self.epoch_credits_view()
            .last()
            .map(|item| item.credits())
            .unwrap_or(0)
    }

    pub fn last_timestamp(&self) -> BlockTimestamp {
        let offset = self.frame.offset(Field::LastTimestamp);
        // SAFETY: `frame` was created from `data`.
        let buffer = &self.data[offset..];
        let mut cursor = std::io::Cursor::new(buffer);
        BlockTimestamp {
            slot: solana_serialize_utils::cursor::read_u64(&mut cursor).unwrap(),
            timestamp: solana_serialize_utils::cursor::read_i64(&mut cursor).unwrap(),
        }
    }

    fn votes_view(&self) -> ListView<VotesFrame> {
        let offset = self.frame.offset(Field::Votes);
        // SAFETY: `frame` was created from `data`.
        ListView::new(self.frame.votes_frame(), &self.data[offset..])
    }

    fn root_slot_view(&self) -> RootSlotView {
        let offset = self.frame.offset(Field::RootSlot);
        // SAFETY: `frame` was created from `data`.
        RootSlotView::new(self.frame.root_slot_frame(), &self.data[offset..])
    }

    fn authorized_voters_view(&self) -> ListView<AuthorizedVotersListFrame> {
        let offset = self.frame.offset(Field::AuthorizedVoters);
        // SAFETY: `frame` was created from `data`.
        ListView::new(self.frame.authorized_voters_frame(), &self.data[offset..])
    }

    fn epoch_credits_view(&self) -> ListView<EpochCreditsListFrame> {
        let offset = self.frame.offset(Field::EpochCredits);
        // SAFETY: `frame` was created from `data`.
        ListView::new(self.frame.epoch_credits_frame(), &self.data[offset..])
    }
}

#[cfg(feature = "dev-context-only-utils")]
impl From<VoteState> for VoteStateView {
    fn from(vote_state: VoteState) -> Self {
        let vote_account_data =
            bincode::serialize(&VoteStateVersions::new_current(vote_state)).unwrap();
        VoteStateView::try_new(Arc::new(vote_account_data)).unwrap()
    }
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
enum VoteStateFrame {
    V1_14_11(VoteStateFrameV1_14_11),
    V3(VoteStateFrameV3),
}

impl VoteStateFrame {
    /// Parse a serialized vote state and verify structure.
    fn try_new(bytes: &[u8]) -> Result<Self> {
        let version = {
            let mut cursor = std::io::Cursor::new(bytes);
            solana_serialize_utils::cursor::read_u32(&mut cursor)
                .map_err(|_err| VoteStateViewError::AccountDataTooSmall)?
        };

        Ok(match version {
            0 => return Err(VoteStateViewError::OldVersion),
            1 => Self::V1_14_11(VoteStateFrameV1_14_11::try_new(bytes)?),
            2 => Self::V3(VoteStateFrameV3::try_new(bytes)?),
            _ => return Err(VoteStateViewError::UnsupportedVersion),
        })
    }

    fn offset(&self, field: Field) -> usize {
        match &self {
            Self::V1_14_11(frame) => frame.field_offset(field),
            Self::V3(frame) => frame.field_offset(field),
        }
    }

    fn votes_frame(&self) -> VotesFrame {
        match &self {
            Self::V1_14_11(frame) => VotesFrame::Lockout(frame.votes_frame),
            Self::V3(frame) => VotesFrame::Landed(frame.votes_frame),
        }
    }

    fn root_slot_frame(&self) -> RootSlotFrame {
        match &self {
            Self::V1_14_11(vote_frame) => vote_frame.root_slot_frame,
            Self::V3(vote_frame) => vote_frame.root_slot_frame,
        }
    }

    fn authorized_voters_frame(&self) -> AuthorizedVotersListFrame {
        match &self {
            Self::V1_14_11(frame) => frame.authorized_voters_frame,
            Self::V3(frame) => frame.authorized_voters_frame,
        }
    }

    fn epoch_credits_frame(&self) -> EpochCreditsListFrame {
        match &self {
            Self::V1_14_11(frame) => frame.epoch_credits_frame,
            Self::V3(frame) => frame.epoch_credits_frame,
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        arbitrary::{Arbitrary, Unstructured},
        solana_clock::Clock,
        solana_vote_interface::{
            authorized_voters::AuthorizedVoters,
            state::{
                vote_state_1_14_11::VoteState1_14_11, LandedVote, VoteInit, VoteState,
                VoteStateVersions, MAX_EPOCH_CREDITS_HISTORY, MAX_LOCKOUT_HISTORY,
            },
        },
        std::collections::VecDeque,
    };

    fn new_test_vote_state() -> VoteState {
        let mut target_vote_state = VoteState::new(
            &VoteInit {
                node_pubkey: Pubkey::new_unique(),
                authorized_voter: Pubkey::new_unique(),
                authorized_withdrawer: Pubkey::new_unique(),
                commission: 42,
            },
            &Clock::default(),
        );

        target_vote_state
            .set_new_authorized_voter(
                &Pubkey::new_unique(), // authorized_pubkey
                0,                     // current_epoch
                1,                     // target_epoch
                |_| Ok(()),
            )
            .unwrap();

        target_vote_state.root_slot = Some(42);
        target_vote_state.epoch_credits.push((42, 42, 42));
        target_vote_state.last_timestamp = BlockTimestamp {
            slot: 42,
            timestamp: 42,
        };
        for i in 0..MAX_LOCKOUT_HISTORY {
            target_vote_state.votes.push_back(LandedVote {
                latency: i as u8,
                lockout: Lockout::new_with_confirmation_count(i as u64, i as u32),
            });
        }

        target_vote_state
    }

    #[test]
    fn test_vote_state_view_v3() {
        let target_vote_state = new_test_vote_state();
        let target_vote_state_versions =
            VoteStateVersions::Current(Box::new(target_vote_state.clone()));
        let vote_state_buf = bincode::serialize(&target_vote_state_versions).unwrap();
        let vote_state_view = VoteStateView::try_new(Arc::new(vote_state_buf)).unwrap();
        assert_eq_vote_state_v3(&vote_state_view, &target_vote_state);
    }

    #[test]
    fn test_vote_state_view_v3_default() {
        let target_vote_state = VoteState::default();
        let target_vote_state_versions =
            VoteStateVersions::Current(Box::new(target_vote_state.clone()));
        let vote_state_buf = bincode::serialize(&target_vote_state_versions).unwrap();
        let vote_state_view = VoteStateView::try_new(Arc::new(vote_state_buf)).unwrap();
        assert_eq_vote_state_v3(&vote_state_view, &target_vote_state);
    }

    #[test]
    fn test_vote_state_view_v3_arbitrary() {
        // variant
        // provide 4x the minimum struct size in bytes to ensure we typically touch every field
        let struct_bytes_x4 = VoteState::size_of() * 4;
        for _ in 0..100 {
            let raw_data: Vec<u8> = (0..struct_bytes_x4).map(|_| rand::random::<u8>()).collect();
            let mut unstructured = Unstructured::new(&raw_data);

            let mut target_vote_state = VoteState::arbitrary(&mut unstructured).unwrap();
            target_vote_state.votes.truncate(MAX_LOCKOUT_HISTORY);
            target_vote_state
                .epoch_credits
                .truncate(MAX_EPOCH_CREDITS_HISTORY);
            if target_vote_state.authorized_voters().len() >= u8::MAX as usize {
                continue;
            }

            let target_vote_state_versions =
                VoteStateVersions::Current(Box::new(target_vote_state.clone()));
            let vote_state_buf = bincode::serialize(&target_vote_state_versions).unwrap();
            let vote_state_view = VoteStateView::try_new(Arc::new(vote_state_buf)).unwrap();
            assert_eq_vote_state_v3(&vote_state_view, &target_vote_state);
        }
    }

    #[test]
    fn test_vote_state_view_1_14_11() {
        let target_vote_state: VoteState1_14_11 = new_test_vote_state().into();
        let target_vote_state_versions =
            VoteStateVersions::V1_14_11(Box::new(target_vote_state.clone()));
        let vote_state_buf = bincode::serialize(&target_vote_state_versions).unwrap();
        let vote_state_view = VoteStateView::try_new(Arc::new(vote_state_buf)).unwrap();
        assert_eq_vote_state_1_14_11(&vote_state_view, &target_vote_state);
    }

    #[test]
    fn test_vote_state_view_1_14_11_default() {
        let target_vote_state = VoteState1_14_11::default();
        let target_vote_state_versions =
            VoteStateVersions::V1_14_11(Box::new(target_vote_state.clone()));
        let vote_state_buf = bincode::serialize(&target_vote_state_versions).unwrap();
        let vote_state_view = VoteStateView::try_new(Arc::new(vote_state_buf)).unwrap();
        assert_eq_vote_state_1_14_11(&vote_state_view, &target_vote_state);
    }

    #[test]
    fn test_vote_state_view_1_14_11_arbitrary() {
        // variant
        // provide 4x the minimum struct size in bytes to ensure we typically touch every field
        let struct_bytes_x4 = std::mem::size_of::<VoteState1_14_11>() * 4;
        for _ in 0..100 {
            let raw_data: Vec<u8> = (0..struct_bytes_x4).map(|_| rand::random::<u8>()).collect();
            let mut unstructured = Unstructured::new(&raw_data);

            let mut target_vote_state = VoteState1_14_11::arbitrary(&mut unstructured).unwrap();
            target_vote_state.votes.truncate(MAX_LOCKOUT_HISTORY);
            target_vote_state
                .epoch_credits
                .truncate(MAX_EPOCH_CREDITS_HISTORY);
            if target_vote_state.authorized_voters.len() >= u8::MAX as usize {
                let (&first, &voter) = target_vote_state.authorized_voters.first().unwrap();
                let mut authorized_voters = AuthorizedVoters::new(first, voter);
                for (epoch, pubkey) in target_vote_state.authorized_voters.iter().skip(1).take(10) {
                    authorized_voters.insert(*epoch, *pubkey);
                }
                target_vote_state.authorized_voters = authorized_voters;
            }

            let target_vote_state_versions =
                VoteStateVersions::V1_14_11(Box::new(target_vote_state.clone()));
            let vote_state_buf = bincode::serialize(&target_vote_state_versions).unwrap();
            let vote_state_view = VoteStateView::try_new(Arc::new(vote_state_buf)).unwrap();
            assert_eq_vote_state_1_14_11(&vote_state_view, &target_vote_state);
        }
    }

    fn assert_eq_vote_state_v3(vote_state_view: &VoteStateView, vote_state: &VoteState) {
        assert_eq!(vote_state_view.node_pubkey(), &vote_state.node_pubkey);
        assert_eq!(vote_state_view.commission(), vote_state.commission);
        let view_votes = vote_state_view.votes_iter().collect::<Vec<_>>();
        let state_votes = vote_state
            .votes
            .iter()
            .map(|vote| vote.lockout)
            .collect::<Vec<_>>();
        assert_eq!(view_votes, state_votes);
        assert_eq!(
            vote_state_view.last_lockout(),
            vote_state.last_lockout().copied()
        );
        assert_eq!(
            vote_state_view.last_voted_slot(),
            vote_state.last_voted_slot(),
        );
        assert_eq!(vote_state_view.root_slot(), vote_state.root_slot);

        if let Some((first_voter_epoch, first_voter)) = vote_state.authorized_voters().first() {
            assert_eq!(
                vote_state_view.get_authorized_voter(*first_voter_epoch),
                Some(first_voter)
            );

            let (last_voter_epoch, last_voter) = vote_state.authorized_voters().last().unwrap();
            assert_eq!(
                vote_state_view.get_authorized_voter(*last_voter_epoch),
                Some(last_voter)
            );
            assert_eq!(
                vote_state_view.get_authorized_voter(u64::MAX),
                Some(last_voter)
            );
        } else {
            assert_eq!(vote_state_view.get_authorized_voter(u64::MAX), None);
        }

        assert_eq!(
            vote_state_view.num_epoch_credits(),
            vote_state.epoch_credits.len()
        );
        let view_credits: Vec<(Epoch, u64, u64)> = vote_state_view
            .epoch_credits_iter()
            .map(Into::into)
            .collect::<Vec<_>>();
        assert_eq!(view_credits, vote_state.epoch_credits);

        assert_eq!(
            vote_state_view.credits(),
            vote_state.epoch_credits.last().map(|x| x.1).unwrap_or(0)
        );
        assert_eq!(vote_state_view.last_timestamp(), vote_state.last_timestamp);
    }

    fn assert_eq_vote_state_1_14_11(
        vote_state_view: &VoteStateView,
        vote_state: &VoteState1_14_11,
    ) {
        assert_eq!(vote_state_view.node_pubkey(), &vote_state.node_pubkey);
        assert_eq!(vote_state_view.commission(), vote_state.commission);
        let view_votes = vote_state_view.votes_iter().collect::<VecDeque<_>>();
        assert_eq!(view_votes, vote_state.votes);
        assert_eq!(
            vote_state_view.last_lockout(),
            vote_state.votes.back().copied()
        );
        assert_eq!(
            vote_state_view.last_voted_slot(),
            vote_state.votes.back().map(|lockout| lockout.slot()),
        );
        assert_eq!(vote_state_view.root_slot(), vote_state.root_slot);

        if let Some((first_voter_epoch, first_voter)) = vote_state.authorized_voters.first() {
            assert_eq!(
                vote_state_view.get_authorized_voter(*first_voter_epoch),
                Some(first_voter)
            );

            let (last_voter_epoch, last_voter) = vote_state.authorized_voters.last().unwrap();
            assert_eq!(
                vote_state_view.get_authorized_voter(*last_voter_epoch),
                Some(last_voter)
            );
            assert_eq!(
                vote_state_view.get_authorized_voter(u64::MAX),
                Some(last_voter)
            );
        } else {
            assert_eq!(vote_state_view.get_authorized_voter(u64::MAX), None);
        }

        assert_eq!(
            vote_state_view.num_epoch_credits(),
            vote_state.epoch_credits.len()
        );
        let view_credits: Vec<(Epoch, u64, u64)> = vote_state_view
            .epoch_credits_iter()
            .map(Into::into)
            .collect::<Vec<_>>();
        assert_eq!(view_credits, vote_state.epoch_credits);

        assert_eq!(
            vote_state_view.credits(),
            vote_state.epoch_credits.last().map(|x| x.1).unwrap_or(0)
        );
        assert_eq!(vote_state_view.last_timestamp(), vote_state.last_timestamp);
    }

    #[test]
    fn test_vote_state_view_too_small() {
        for i in 0..4 {
            let vote_data = Arc::new(vec![0; i]);
            let vote_state_view_err = VoteStateView::try_new(vote_data).unwrap_err();
            assert_eq!(vote_state_view_err, VoteStateViewError::AccountDataTooSmall);
        }
    }

    #[test]
    fn test_vote_state_view_old_version() {
        let vote_data = Arc::new(0u32.to_le_bytes().to_vec());
        let vote_state_view_err = VoteStateView::try_new(vote_data).unwrap_err();
        assert_eq!(vote_state_view_err, VoteStateViewError::OldVersion);
    }

    #[test]
    fn test_vote_state_view_unsupported_version() {
        let vote_data = Arc::new(3u32.to_le_bytes().to_vec());
        let vote_state_view_err = VoteStateView::try_new(vote_data).unwrap_err();
        assert_eq!(vote_state_view_err, VoteStateViewError::UnsupportedVersion);
    }
}
