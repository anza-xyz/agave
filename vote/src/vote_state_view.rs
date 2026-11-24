#[cfg(feature = "dev-context-only-utils")]
use solana_vote_interface::state::{
    VoteStateV3, VoteStateV4, VoteStateVersions, vote_state_v5::VOTE_STATE_V5_SIZE,
};
use {
    self::{
        field_frames::{
            AccumulatedLamportsItem, AccumulatedLamportsListFrame, AuthorizedVotersListFrame,
            BlsPubkeyCompressedFrame, BlsPubkeyCompressedView, PendingDelegatorRewardsView,
            RootSlotFrame, RootSlotView, VotesFrame,
        },
        frame_v1_14_11::VoteStateFrameV1_14_11,
        frame_v3::VoteStateFrameV3,
        list_view::ListView,
    },
    core::fmt::Debug,
    field_frames::{CommissionFrame, CommissionView},
    frame_v4::VoteStateFrameV4,
    solana_clock::{Epoch, Slot},
    solana_pubkey::Pubkey,
    solana_vote_interface::state::{
        BLS_PUBLIC_KEY_COMPRESSED_SIZE, BlockTimestamp, Lockout,
        vote_state_v5::{CommissionScheduleRef, Location, VOTE_STATE_V5_DISCRIMINANT, VoteStateV5},
    },
    std::sync::Arc,
};

mod field_frames;
mod frame_v1_14_11;
mod frame_v3;
pub(crate) mod frame_v4;
mod list_view;

#[derive(Debug, PartialEq, Eq)]
pub enum VoteStateViewError {
    AccountDataTooSmall,
    InvalidVotesLength,
    InvalidRootSlotOption,
    InvalidBlsPubkeyCompressedOption,
    InvalidAuthorizedVotersLength,
    InvalidEpochCreditsLength,
    OldVersion,
    UnsupportedVersion,
    AccountDataSizeIncorrect,
    InvalidVoteStateV5,
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

enum Simd185Field {
    InflationRewardsCollector,
    BlockRevenueCollector,
    BlockRevenueCommission,
    PendingDelegatorRewards,
    BlsPubkeyCompressed,
}

/// A view into a serialized VoteState.
///
/// This struct provides access to the VoteState data without
/// deserializing it. This is done by parsing and caching metadata
/// about the layout of the serialized VoteState.
#[derive(Debug, Clone)]
pub struct VoteStateView {
    data: Arc<Vec<u8>>,
    frame: VoteStateFrame,
}

impl VoteStateView {
    pub fn try_new(data: Arc<Vec<u8>>) -> Result<Self> {
        let frame = VoteStateFrame::try_new(data.as_ref())?;
        Ok(Self { data, frame })
    }

    /// The v5 state, if this view is over a v5 account.
    #[inline]
    fn v5(&self) -> Option<&VoteStateV5> {
        match &self.frame {
            VoteStateFrame::V5(state) => Some(state),
            _ => None,
        }
    }

    /// Location registered in a v5 account; older versions have none.
    pub fn location(&self) -> Option<Location> {
        self.v5().map(|state| state.location)
    }

    pub fn node_pubkey(&self) -> &Pubkey {
        if let Some(state) = self.v5() {
            return &state.node_pubkey;
        }
        let offset = self.frame.offset(Field::NodePubkey);
        // SAFETY: `frame` was created from `data`.
        unsafe { &*(self.data.as_ptr().add(offset) as *const Pubkey) }
    }

    pub fn commission(&self) -> u8 {
        if self.v5().is_some() {
            return (self.inflation_rewards_commission() / 100).min(u8::MAX as u16) as u8;
        }
        self.inflation_rewards_commission_view()
            .commission_percent()
    }

    pub fn block_revenue_collector(&self) -> Option<&Pubkey> {
        if let Some(state) = self.v5() {
            return Some(&state.block_revenue_collector);
        }
        let offset = self
            .frame
            .simd185_field_offset(Simd185Field::BlockRevenueCollector)?;
        // SAFETY: `frame` was created from `data`.
        unsafe { Some(&*(self.data.as_ptr().add(offset) as *const Pubkey)) }
    }

    pub fn inflation_rewards_collector(&self) -> Option<&Pubkey> {
        if let Some(state) = self.v5() {
            return Some(&state.inflation_rewards_collector);
        }
        let offset = self
            .frame
            .simd185_field_offset(Simd185Field::InflationRewardsCollector)?;
        // SAFETY: `frame` was created from `data`.
        unsafe { Some(&*(self.data.as_ptr().add(offset) as *const Pubkey)) }
    }

    pub fn inflation_rewards_commission(&self) -> u16 {
        if let Some(state) = self.v5() {
            return latest_commission_bps(state.inflation_rewards_commission_ref(), 0);
        }
        self.inflation_rewards_commission_view().commission_bps()
    }

    pub fn block_revenue_commission(&self) -> u16 {
        // Pre-v4 states have no block revenue commission, so the validator keeps all
        // block revenue, which is 100%.
        const NO_BLOCK_REVENUE_COMMISSION_BPS: u16 = 10_000;
        if let Some(state) = self.v5() {
            return latest_commission_bps(
                state.block_revenue_commission_ref(),
                NO_BLOCK_REVENUE_COMMISSION_BPS,
            );
        }
        self.block_revenue_commission_view()
            .map(|view| view.commission_bps())
            .unwrap_or(NO_BLOCK_REVENUE_COMMISSION_BPS)
    }

    pub fn pending_delegator_rewards(&self) -> u64 {
        if let Some(state) = self.v5() {
            return state.pending_delegator_rewards;
        }
        self.pending_delegator_rewards_view()
            .map(|view| view.value())
            .unwrap_or(0)
    }

    pub fn bls_pubkey_compressed(&self) -> Option<[u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE]> {
        if let Some(state) = self.v5() {
            return Some(state.bls_pubkey_compressed);
        }
        self.bls_pubkey_compressed_view()
            .and_then(|view| view.pubkey())
    }

    pub fn votes_iter(&self) -> impl Iterator<Item = Lockout> + '_ {
        let v5_vote = self.v5().and_then(|_| self.last_lockout());
        let tower = self.v5().is_none().then(|| self.votes_view());
        v5_vote
            .into_iter()
            .chain(tower.into_iter().flatten().map(|vote| {
                Lockout::new_with_confirmation_count(vote.slot(), vote.confirmation_count())
            }))
    }

    #[inline]
    pub fn votes_len(&self) -> usize {
        match self.v5() {
            Some(state) => usize::from(state.latest_vote().is_some()),
            None => self.votes_view().len(),
        }
    }

    pub fn last_lockout(&self) -> Option<Lockout> {
        if let Some(state) = self.v5() {
            return state.latest_vote().map(Lockout::new);
        }
        self.votes_view().last().map(|item| {
            Lockout::new_with_confirmation_count(item.slot(), item.confirmation_count())
        })
    }

    pub fn last_voted_slot(&self) -> Option<Slot> {
        if let Some(state) = self.v5() {
            return state.latest_vote();
        }
        self.votes_view().last().map(|item| item.slot())
    }

    pub fn root_slot(&self) -> Option<Slot> {
        if let Some(state) = self.v5() {
            return state.root_slot();
        }
        self.root_slot_view().root_slot()
    }

    pub fn get_authorized_voter(&self, epoch: Epoch) -> Option<&Pubkey> {
        if let Some(state) = self.v5() {
            return state
                .authorized_voters_ref()
                .0
                .iter()
                .rev()
                .find(|voter| voter.epoch <= epoch)
                .map(|voter| &voter.pubkey);
        }
        self.authorized_voters_view().get_authorized_voter(epoch)
    }

    pub fn num_accumulated_lamports(&self) -> usize {
        if let Some(state) = self.v5() {
            return state.accumulated_lamports_ref().0.len();
        }
        self.accumulated_lamports_view().len()
    }

    /// Entries are oldest first, for every version.
    pub fn accumulated_lamports_iter(&self) -> impl Iterator<Item = &AccumulatedLamportsItem> + '_ {
        let v5_entries = self
            .v5()
            .map(|state| state.accumulated_lamports_ref().0)
            .unwrap_or_default();
        let tower = self
            .v5()
            .is_none()
            .then(|| self.accumulated_lamports_view());
        v5_entries
            .iter()
            .map(AccumulatedLamportsItem::from_accumulated_lamports)
            .chain(tower.into_iter().flatten())
    }

    pub fn accumulated_lamports(&self) -> u64 {
        if let Some(state) = self.v5() {
            return state
                .accumulated_lamports_ref()
                .0
                .last()
                .map(|entry| entry.lamports)
                .unwrap_or(0);
        }
        self.accumulated_lamports_view()
            .last()
            .map(|item| item.lamports())
            .unwrap_or(0)
    }

    pub fn last_timestamp(&self) -> BlockTimestamp {
        if let Some(state) = self.v5() {
            return BlockTimestamp {
                slot: state.last_timestamp.slot,
                timestamp: state.last_timestamp.timestamp,
            };
        }
        let offset = self.frame.offset(Field::LastTimestamp);
        // SAFETY: `frame` was created from `data`.
        let buffer = &self.data[offset..];
        let mut cursor = std::io::Cursor::new(buffer);
        BlockTimestamp {
            slot: solana_serialize_utils::cursor::read_u64(&mut cursor).unwrap(),
            timestamp: solana_serialize_utils::cursor::read_i64(&mut cursor).unwrap(),
        }
    }

    fn inflation_rewards_commission_view(&self) -> CommissionView<'_> {
        let offset = self.frame.offset(Field::Commission);
        // SAFETY: `frame` was created from `data`.
        CommissionView::new(self.frame.commission_frame(), &self.data[offset..])
    }

    fn block_revenue_commission_view(&self) -> Option<CommissionView<'_>> {
        let offset = self
            .frame
            .simd185_field_offset(Simd185Field::BlockRevenueCommission)?;
        // SAFETY: `frame` was created from `data`.
        Some(CommissionView::new(
            CommissionFrame::new_bps(),
            &self.data[offset..],
        ))
    }

    fn pending_delegator_rewards_view(&self) -> Option<PendingDelegatorRewardsView<'_>> {
        let offset = self
            .frame
            .simd185_field_offset(Simd185Field::PendingDelegatorRewards)?;
        // SAFETY: `frame` was created from `data`.
        Some(PendingDelegatorRewardsView::new(&self.data[offset..]))
    }

    fn bls_pubkey_compressed_view(&self) -> Option<BlsPubkeyCompressedView<'_>> {
        let offset = self
            .frame
            .simd185_field_offset(Simd185Field::BlsPubkeyCompressed)?;
        let frame = self.frame.bls_pubkey_compressed_frame()?;
        // SAFETY: `frame` was created from `data`.
        Some(BlsPubkeyCompressedView::new(frame, &self.data[offset..]))
    }

    fn votes_view(&self) -> ListView<'_, VotesFrame> {
        let offset = self.frame.offset(Field::Votes);
        // SAFETY: `frame` was created from `data`.
        ListView::new(self.frame.votes_frame(), &self.data[offset..])
    }

    fn root_slot_view(&self) -> RootSlotView<'_> {
        let offset = self.frame.offset(Field::RootSlot);
        // SAFETY: `frame` was created from `data`.
        RootSlotView::new(self.frame.root_slot_frame(), &self.data[offset..])
    }

    fn authorized_voters_view(&self) -> ListView<'_, AuthorizedVotersListFrame> {
        let offset = self.frame.offset(Field::AuthorizedVoters);
        // SAFETY: `frame` was created from `data`.
        ListView::new(self.frame.authorized_voters_frame(), &self.data[offset..])
    }

    fn accumulated_lamports_view(&self) -> ListView<'_, AccumulatedLamportsListFrame> {
        let offset = self.frame.offset(Field::EpochCredits);
        // SAFETY: `frame` was created from `data`.
        ListView::new(
            self.frame.accumulated_lamports_frame(),
            &self.data[offset..],
        )
    }
}

/// Returns the commission in effect, which is the latest change of a v5 schedule.
///
/// The SDK validates 1 to 4 changes, so `default` is unreachable.
fn latest_commission_bps(schedule: CommissionScheduleRef<'_>, default: u16) -> u16 {
    debug_assert!(!schedule.bps.is_empty());
    schedule.bps.last().map_or(default, |bps| bps.get())
}

#[cfg(feature = "dev-context-only-utils")]
impl From<VoteStateV3> for VoteStateView {
    fn from(vote_state: VoteStateV3) -> Self {
        let vote_account_data = wincode::serialize(&VoteStateVersions::new_v3(vote_state)).unwrap();
        VoteStateView::try_new(Arc::new(vote_account_data)).unwrap()
    }
}

#[cfg(feature = "dev-context-only-utils")]
impl From<VoteStateV4> for VoteStateView {
    fn from(vote_state: VoteStateV4) -> Self {
        let vote_account_data = wincode::serialize(&VoteStateVersions::new_v4(vote_state)).unwrap();
        VoteStateView::try_new(Arc::new(vote_account_data)).unwrap()
    }
}

#[cfg(feature = "dev-context-only-utils")]
impl From<VoteStateV5> for VoteStateView {
    fn from(vote_state: VoteStateV5) -> Self {
        let mut vote_account_data = vec![0; VOTE_STATE_V5_SIZE];
        vote_state
            .write_account_data(&mut vote_account_data)
            .unwrap();
        VoteStateView::try_new(Arc::new(vote_account_data)).unwrap()
    }
}

#[derive(Debug, Clone)]
enum VoteStateFrame {
    V1_14_11(VoteStateFrameV1_14_11),
    V3(VoteStateFrameV3),
    V4(VoteStateFrameV4),
    /// Owned copy, so that the account data needs no particular alignment.
    V5(Box<VoteStateV5>),
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
            3 => Self::V4(VoteStateFrameV4::try_new(bytes)?),
            VOTE_STATE_V5_DISCRIMINANT => Self::V5(Box::new(
                VoteStateV5::from_account_data_unaligned(bytes)
                    .map_err(|_| VoteStateViewError::InvalidVoteStateV5)?,
            )),
            _ => return Err(VoteStateViewError::UnsupportedVersion),
        })
    }

    fn offset(&self, field: Field) -> usize {
        match &self {
            Self::V1_14_11(frame) => frame.field_offset(field),
            Self::V3(frame) => frame.field_offset(field),
            Self::V4(frame) => frame.field_offset(field),
            Self::V5(_) => unreachable!("v5 accessors do not use offsets"),
        }
    }

    fn simd185_field_offset(&self, field: Simd185Field) -> Option<usize> {
        match &self {
            Self::V1_14_11(_frame) => None,
            Self::V3(_frame) => None,
            Self::V4(frame) => Some(frame.simd185_field_offset(field)),
            Self::V5(_) => unreachable!("v5 accessors do not use offsets"),
        }
    }

    fn commission_frame(&self) -> CommissionFrame {
        match &self {
            Self::V1_14_11(_) => CommissionFrame::new_percent(),
            Self::V3(_) => CommissionFrame::new_percent(),
            Self::V4(_) => CommissionFrame::new_bps(),
            Self::V5(_) => unreachable!("v5 accessors do not use frames"),
        }
    }

    fn bls_pubkey_compressed_frame(&self) -> Option<BlsPubkeyCompressedFrame> {
        match &self {
            Self::V1_14_11 { .. } | Self::V3 { .. } => None,
            Self::V4(frame) => Some(frame.bls_pubkey_compressed_frame),
            Self::V5(_) => unreachable!("v5 accessors do not use frames"),
        }
    }

    fn votes_frame(&self) -> VotesFrame {
        match &self {
            Self::V1_14_11(frame) => VotesFrame::Lockout(frame.votes_frame),
            Self::V3(frame) => VotesFrame::Landed(frame.votes_frame),
            Self::V4(frame) => VotesFrame::Landed(frame.votes_frame),
            Self::V5(_) => unreachable!("v5 accessors do not use frames"),
        }
    }

    fn root_slot_frame(&self) -> RootSlotFrame {
        match &self {
            Self::V1_14_11(vote_frame) => vote_frame.root_slot_frame,
            Self::V3(vote_frame) => vote_frame.root_slot_frame,
            Self::V4(vote_frame) => vote_frame.root_slot_frame,
            Self::V5(_) => unreachable!("v5 accessors do not use frames"),
        }
    }

    fn authorized_voters_frame(&self) -> AuthorizedVotersListFrame {
        match &self {
            Self::V1_14_11(frame) => frame.authorized_voters_frame,
            Self::V3(frame) => frame.authorized_voters_frame,
            Self::V4(frame) => frame.authorized_voters_frame,
            Self::V5(_) => unreachable!("v5 accessors do not use frames"),
        }
    }

    fn accumulated_lamports_frame(&self) -> AccumulatedLamportsListFrame {
        match &self {
            Self::V1_14_11(frame) => frame.epoch_credits_frame,
            Self::V3(frame) => frame.epoch_credits_frame,
            Self::V4(frame) => frame.epoch_credits_frame,
            Self::V5(_) => unreachable!("v5 accessors do not use frames"),
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
                LandedVote, MAX_EPOCH_CREDITS_HISTORY, MAX_LOCKOUT_HISTORY, VoteInit,
                VoteState1_14_11, VoteStateV3, VoteStateV4, VoteStateVersions,
            },
        },
        std::collections::VecDeque,
    };

    fn new_test_vote_state_v4() -> VoteStateV4 {
        let votes = (0..MAX_LOCKOUT_HISTORY)
            .map(|i| LandedVote {
                latency: i as u8,
                lockout: Lockout::new_with_confirmation_count(i as u64, i as u32),
            })
            .collect();

        VoteStateV4 {
            node_pubkey: Pubkey::new_unique(),
            authorized_withdrawer: Pubkey::new_unique(),
            inflation_rewards_collector: Pubkey::new_unique(),
            block_revenue_collector: Pubkey::new_unique(),
            inflation_rewards_commission_bps: 42,
            block_revenue_commission_bps: 42,
            pending_delegator_rewards: 42,
            bls_pubkey_compressed: Some([42; BLS_PUBLIC_KEY_COMPRESSED_SIZE]),
            votes,
            root_slot: Some(42),
            authorized_voters: AuthorizedVoters::new(42, Pubkey::new_unique()),
            epoch_credits: vec![(42, 42, 42)],
            last_timestamp: BlockTimestamp {
                slot: 42,
                timestamp: 42,
            },
        }
    }

    fn new_test_vote_state_v3() -> VoteStateV3 {
        let mut target_vote_state = VoteStateV3::new(
            &VoteInit {
                node_pubkey: Pubkey::new_unique(),
                authorized_voter: Pubkey::new_unique(),
                authorized_withdrawer: Pubkey::new_unique(),
                commission: 42,
            },
            &Clock::default(),
        );

        // Add another authorized voter for epoch 1
        target_vote_state
            .authorized_voters
            .insert(1, Pubkey::new_unique());

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
    fn test_vote_state_view_v4() {
        let target_vote_state = new_test_vote_state_v4();
        let target_vote_state_versions = VoteStateVersions::new_v4(target_vote_state.clone());
        let vote_state_buf = wincode::serialize(&target_vote_state_versions).unwrap();
        let vote_state_view = VoteStateView::try_new(Arc::new(vote_state_buf)).unwrap();
        assert_eq_vote_state_v4(&vote_state_view, &target_vote_state);
    }

    #[test]
    fn test_vote_state_view_v4_default() {
        let target_vote_state = VoteStateV4::default();
        let target_vote_state_versions = VoteStateVersions::new_v4(target_vote_state.clone());
        let vote_state_buf = wincode::serialize(&target_vote_state_versions).unwrap();
        let vote_state_view = VoteStateView::try_new(Arc::new(vote_state_buf)).unwrap();
        assert_eq_vote_state_v4(&vote_state_view, &target_vote_state);
    }

    #[test]
    fn test_vote_state_view_v4_arbitrary() {
        // variant
        // provide 4x the minimum struct size in bytes to ensure we typically touch every field
        let struct_bytes_x4 = VoteStateV3::size_of() * 4;
        for _ in 0..100 {
            let raw_data: Vec<u8> = (0..struct_bytes_x4).map(|_| rand::random::<u8>()).collect();
            let mut unstructured = Unstructured::new(&raw_data);

            let mut target_vote_state = VoteStateV4::arbitrary(&mut unstructured).unwrap();
            target_vote_state.votes.truncate(MAX_LOCKOUT_HISTORY);
            target_vote_state
                .epoch_credits
                .truncate(MAX_EPOCH_CREDITS_HISTORY);
            if target_vote_state.authorized_voters.len() >= u8::MAX as usize {
                continue;
            }

            let target_vote_state_versions = VoteStateVersions::new_v4(target_vote_state.clone());
            let vote_state_buf = wincode::serialize(&target_vote_state_versions).unwrap();
            let vote_state_view = VoteStateView::try_new(Arc::new(vote_state_buf)).unwrap();
            assert_eq_vote_state_v4(&vote_state_view, &target_vote_state);
        }
    }
    #[test]
    fn test_vote_state_view_v3() {
        let target_vote_state = new_test_vote_state_v3();
        let target_vote_state_versions = VoteStateVersions::V3(Box::new(target_vote_state.clone()));
        let vote_state_buf = wincode::serialize(&target_vote_state_versions).unwrap();
        let vote_state_view = VoteStateView::try_new(Arc::new(vote_state_buf)).unwrap();
        assert_eq_vote_state_v3(&vote_state_view, &target_vote_state);
    }

    #[test]
    fn test_vote_state_view_v3_default() {
        let target_vote_state = VoteStateV3::default();
        let target_vote_state_versions = VoteStateVersions::V3(Box::new(target_vote_state.clone()));
        let vote_state_buf = wincode::serialize(&target_vote_state_versions).unwrap();
        let vote_state_view = VoteStateView::try_new(Arc::new(vote_state_buf)).unwrap();
        assert_eq_vote_state_v3(&vote_state_view, &target_vote_state);
    }

    #[test]
    fn test_vote_state_view_v3_arbitrary() {
        // variant
        // provide 4x the minimum struct size in bytes to ensure we typically touch every field
        let struct_bytes_x4 = VoteStateV3::size_of() * 4;
        for _ in 0..100 {
            let raw_data: Vec<u8> = (0..struct_bytes_x4).map(|_| rand::random::<u8>()).collect();
            let mut unstructured = Unstructured::new(&raw_data);

            let mut target_vote_state = VoteStateV3::arbitrary(&mut unstructured).unwrap();
            target_vote_state.votes.truncate(MAX_LOCKOUT_HISTORY);
            target_vote_state
                .epoch_credits
                .truncate(MAX_EPOCH_CREDITS_HISTORY);
            if target_vote_state.authorized_voters.len() >= u8::MAX as usize {
                continue;
            }

            let target_vote_state_versions =
                VoteStateVersions::V3(Box::new(target_vote_state.clone()));
            let vote_state_buf = wincode::serialize(&target_vote_state_versions).unwrap();
            let vote_state_view = VoteStateView::try_new(Arc::new(vote_state_buf)).unwrap();
            assert_eq_vote_state_v3(&vote_state_view, &target_vote_state);
        }
    }

    #[test]
    fn test_vote_state_view_1_14_11() {
        let target_vote_state: VoteState1_14_11 = new_test_vote_state_v3().into();
        let target_vote_state_versions =
            VoteStateVersions::V1_14_11(Box::new(target_vote_state.clone()));
        let vote_state_buf = wincode::serialize(&target_vote_state_versions).unwrap();
        let vote_state_view = VoteStateView::try_new(Arc::new(vote_state_buf)).unwrap();
        assert_eq_vote_state_1_14_11(&vote_state_view, &target_vote_state);
    }

    #[test]
    fn test_vote_state_view_1_14_11_default() {
        let target_vote_state = VoteState1_14_11::default();
        let target_vote_state_versions =
            VoteStateVersions::V1_14_11(Box::new(target_vote_state.clone()));
        let vote_state_buf = wincode::serialize(&target_vote_state_versions).unwrap();
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
            let vote_state_buf = wincode::serialize(&target_vote_state_versions).unwrap();
            let vote_state_view = VoteStateView::try_new(Arc::new(vote_state_buf)).unwrap();
            assert_eq_vote_state_1_14_11(&vote_state_view, &target_vote_state);
        }
    }

    fn assert_eq_vote_state_v4(vote_state_view: &VoteStateView, vote_state: &VoteStateV4) {
        assert_eq!(vote_state_view.node_pubkey(), &vote_state.node_pubkey);
        assert_eq!(
            vote_state_view.inflation_rewards_collector(),
            Some(&vote_state.inflation_rewards_collector)
        );
        assert_eq!(
            vote_state_view.block_revenue_collector(),
            Some(&vote_state.block_revenue_collector)
        );
        assert_eq!(
            vote_state_view.inflation_rewards_commission(),
            vote_state.inflation_rewards_commission_bps
        );
        assert_eq!(
            vote_state_view.block_revenue_commission(),
            vote_state.block_revenue_commission_bps
        );
        assert_eq!(
            vote_state_view.pending_delegator_rewards(),
            vote_state.pending_delegator_rewards
        );
        assert_eq!(
            vote_state_view.bls_pubkey_compressed(),
            vote_state.bls_pubkey_compressed
        );
        let view_votes = vote_state_view.votes_iter().collect::<Vec<_>>();
        let state_votes = vote_state
            .votes
            .iter()
            .map(|vote| vote.lockout)
            .collect::<Vec<_>>();
        assert_eq!(view_votes, state_votes);
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
            vote_state_view.num_accumulated_lamports(),
            vote_state.epoch_credits.len()
        );
        let view_credits: Vec<(Epoch, u64, u64)> = vote_state_view
            .accumulated_lamports_iter()
            .map(Into::into)
            .collect::<Vec<_>>();
        assert_eq!(view_credits, vote_state.epoch_credits);

        assert_eq!(
            vote_state_view.accumulated_lamports(),
            vote_state.epoch_credits.last().map(|x| x.1).unwrap_or(0)
        );
        assert_eq!(vote_state_view.last_timestamp(), vote_state.last_timestamp);
    }

    fn assert_eq_vote_state_v3(vote_state_view: &VoteStateView, vote_state: &VoteStateV3) {
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
            vote_state.votes.back().map(|v| v.lockout)
        );
        assert_eq!(
            vote_state_view.last_voted_slot(),
            vote_state.votes.back().map(|v| v.lockout.slot()),
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
            vote_state_view.num_accumulated_lamports(),
            vote_state.epoch_credits.len()
        );
        let view_credits: Vec<(Epoch, u64, u64)> = vote_state_view
            .accumulated_lamports_iter()
            .map(Into::into)
            .collect::<Vec<_>>();
        assert_eq!(view_credits, vote_state.epoch_credits);

        assert_eq!(
            vote_state_view.accumulated_lamports(),
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
            vote_state_view.num_accumulated_lamports(),
            vote_state.epoch_credits.len()
        );
        let view_credits: Vec<(Epoch, u64, u64)> = vote_state_view
            .accumulated_lamports_iter()
            .map(Into::into)
            .collect::<Vec<_>>();
        assert_eq!(view_credits, vote_state.epoch_credits);

        assert_eq!(
            vote_state_view.accumulated_lamports(),
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
        let vote_data = Arc::new(5u32.to_le_bytes().to_vec());
        let vote_state_view_err = VoteStateView::try_new(vote_data).unwrap_err();
        assert_eq!(vote_state_view_err, VoteStateViewError::UnsupportedVersion);
    }

    #[test]
    fn test_commission_frame_dispatch_cross_version() {
        // Verify inflation_rewards_commission() returns correct bps for
        // all vote state versions.
        for commission_pct in [0u8, 1, 50, 100, 255] {
            let expected_bps = commission_pct as u16 * 100;

            // V1_14_11
            {
                let state = VoteState1_14_11 {
                    commission: commission_pct,
                    authorized_voters: AuthorizedVoters::new(0, Pubkey::default()),
                    ..Default::default()
                };
                let versioned = VoteStateVersions::V1_14_11(Box::new(state));
                let buf = Arc::new(wincode::serialize(&versioned).unwrap());
                let view = VoteStateView::try_new(buf).unwrap();
                assert_eq!(view.commission(), commission_pct);
                assert_eq!(view.inflation_rewards_commission(), expected_bps);
            }

            // V3
            {
                let state = VoteStateV3::new(
                    &VoteInit {
                        node_pubkey: Pubkey::default(),
                        authorized_voter: Pubkey::default(),
                        authorized_withdrawer: Pubkey::default(),
                        commission: commission_pct,
                    },
                    &Clock::default(),
                );
                let versioned = VoteStateVersions::V3(Box::new(state));
                let buf = Arc::new(wincode::serialize(&versioned).unwrap());
                let view = VoteStateView::try_new(buf).unwrap();
                assert_eq!(view.commission(), commission_pct);
                assert_eq!(view.inflation_rewards_commission(), expected_bps);
            }

            // V4
            {
                let state = VoteStateV4 {
                    inflation_rewards_commission_bps: expected_bps,
                    ..VoteStateV4::default()
                };
                let versioned = VoteStateVersions::new_v4(state);
                let buf = Arc::new(wincode::serialize(&versioned).unwrap());
                let view = VoteStateView::try_new(buf).unwrap();
                assert_eq!(
                    view.commission(),
                    (expected_bps / 100).min(u8::MAX as u16) as u8
                );
                assert_eq!(view.inflation_rewards_commission(), expected_bps);
            }
        }
    }

    #[test]
    fn test_vote_state_view_v4_commission_clamps() {
        // The VoteStateView commission() getter must clamp values
        // > u8::MAX to u8::MAX for V4 accounts with large bps.
        for (bps, expected) in [
            (0u16, 0u8),
            (10_000, 100),
            (25_500, 255),
            (25_600, 255),
            (u16::MAX, 255),
        ] {
            let state = VoteStateV4 {
                inflation_rewards_commission_bps: bps,
                ..VoteStateV4::default()
            };
            let versioned = VoteStateVersions::new_v4(state);
            let buf = Arc::new(wincode::serialize(&versioned).unwrap());
            let view = VoteStateView::try_new(buf).unwrap();
            assert_eq!(view.commission(), expected);
        }
    }

    #[test]
    fn test_vote_state_view_v4_trailing_nonzero_bytes() {
        // Verify that trailing non-zero (garbage) bytes do not affect
        // VoteStateView accessor results.
        let target_vote_state = new_test_vote_state_v4();
        let versioned = VoteStateVersions::new_v4(target_vote_state.clone());
        let mut buf = wincode::serialize(&versioned).unwrap();
        buf.extend_from_slice(&[0xAB; 100]);
        let view = VoteStateView::try_new(Arc::new(buf)).unwrap();
        assert_eq_vote_state_v4(&view, &target_vote_state);
    }

    #[test]
    fn test_vote_state_view_v4_trailing_zero_vs_nonzero_equal() {
        // Two V4 accounts — one with trailing zeros, one with trailing
        // garbage — should produce identical accessor results.
        let target_vote_state = new_test_vote_state_v4();
        let versioned = VoteStateVersions::new_v4(target_vote_state);
        let base = wincode::serialize(&versioned).unwrap();

        let mut buf_zero = base.clone();
        buf_zero.extend_from_slice(&[0x00; 100]);
        let view_zero = VoteStateView::try_new(Arc::new(buf_zero)).unwrap();

        let mut buf_garbage = base;
        buf_garbage.extend_from_slice(&[0xFF; 100]);
        let view_garbage = VoteStateView::try_new(Arc::new(buf_garbage)).unwrap();

        assert_eq!(view_zero.node_pubkey(), view_garbage.node_pubkey());
        assert_eq!(view_zero.commission(), view_garbage.commission());
        assert_eq!(
            view_zero.inflation_rewards_commission(),
            view_garbage.inflation_rewards_commission()
        );
        assert_eq!(view_zero.root_slot(), view_garbage.root_slot());
        assert_eq!(
            view_zero.num_accumulated_lamports(),
            view_garbage.num_accumulated_lamports()
        );
    }

    #[test]
    fn test_v5_view_matches_v4_view_after_conversion() {
        use solana_vote_interface::state::vote_state_v5::Location;

        for credits_len in [0u64, 1, 3, 5, 8] {
            let mut v4 = new_test_vote_state_v4();
            v4.epoch_credits = (0..credits_len)
                .map(|e| (e + 10, (e + 1) * 100, e * 100))
                .collect();
            v4.authorized_voters = AuthorizedVoters::new(3, Pubkey::new_unique());
            v4.authorized_voters.insert(7, Pubkey::new_unique());
            let v4_view = VoteStateView::from(v4.clone());
            let v5 = VoteStateV5::from_vote_state_v4(
                v4.clone(),
                Location::try_new(4051542, 618526, 4870645).unwrap(),
                20,
            )
            .unwrap();
            let v5_view = VoteStateView::from(v5);

            assert_eq!(v5_view.node_pubkey(), v4_view.node_pubkey());
            assert_eq!(
                v5_view.inflation_rewards_collector(),
                v4_view.inflation_rewards_collector()
            );
            assert_eq!(
                v5_view.block_revenue_collector(),
                v4_view.block_revenue_collector()
            );
            assert_eq!(v5_view.commission(), v4_view.commission());
            assert_eq!(
                v5_view.inflation_rewards_commission(),
                v4_view.inflation_rewards_commission()
            );
            assert_eq!(
                v5_view.block_revenue_commission(),
                v4_view.block_revenue_commission()
            );
            assert_eq!(
                v5_view.pending_delegator_rewards(),
                v4_view.pending_delegator_rewards()
            );
            assert_eq!(
                v5_view.bls_pubkey_compressed(),
                v4_view.bls_pubkey_compressed()
            );
            assert_eq!(v5_view.root_slot(), v4_view.root_slot());
            assert_eq!(v5_view.last_voted_slot(), v4_view.last_voted_slot());
            assert_eq!(v5_view.last_timestamp(), v4_view.last_timestamp());
            assert_eq!(
                v5_view.accumulated_lamports(),
                v4_view.accumulated_lamports()
            );
            for epoch in [0, 3, 6, 7, 100] {
                assert_eq!(
                    v5_view.get_authorized_voter(epoch),
                    v4_view.get_authorized_voter(epoch)
                );
            }
            let tail = |view: &VoteStateView| -> Vec<(Epoch, u64, u64)> {
                let all: Vec<_> = view.accumulated_lamports_iter().map(Into::into).collect();
                all[all.len().saturating_sub(5)..].to_vec()
            };
            assert_eq!(tail(&v5_view), tail(&v4_view));
            assert_eq!(v5_view.num_accumulated_lamports(), tail(&v4_view).len());
        }
    }
}
