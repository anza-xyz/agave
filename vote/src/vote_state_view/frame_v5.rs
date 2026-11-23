use {
    super::{Field, Result, Simd185Field, VoteStateViewError},
    solana_clock::Slot,
    solana_vote_interface::state::vote_state_v5::{Location, VoteStateV5},
};

/// Validated metadata for V5's fixed layout. Account fields are read from the
/// original bytes, including when the account buffer is not aligned for V5.
#[derive(Debug, Clone)]
pub(super) struct VoteStateFrameV5 {
    pub location: Location,
    pub latest_vote: Option<Slot>,
    pub root_slot: Option<Slot>,
    pub authorized_voters_len: u8,
    pub accumulated_lamports_len: u8,
    inflation_commission_offset: usize,
    block_commission_offset: usize,
}

impl VoteStateFrameV5 {
    pub fn try_new(bytes: &[u8]) -> Result<Self> {
        let state = VoteStateV5::from_account_data_unaligned(bytes)
            .map_err(|_| VoteStateViewError::InvalidVoteStateV5)?;
        Ok(Self {
            location: state.location,
            latest_vote: state.latest_vote(),
            root_slot: state.root_slot(),
            authorized_voters_len: state.authorized_voters_ref().0.len() as u8,
            accumulated_lamports_len: state.accumulated_lamports_ref().0.len() as u8,
            inflation_commission_offset: 192
                + (state.inflation_rewards_commission_ref().bps.len() - 1) * 2,
            block_commission_offset: 232 + (state.block_revenue_commission_ref().bps.len() - 1) * 2,
        })
    }

    pub fn field_offset(&self, field: Field) -> usize {
        match field {
            Field::NodePubkey => std::mem::offset_of!(VoteStateV5, node_pubkey),
            Field::Commission => self.inflation_commission_offset,
            // V5 has no tower. Its iterator's empty frame never reads this field.
            Field::Votes => 0,
            Field::RootSlot => 296,
            Field::AuthorizedVoters => 312,
            Field::EpochCredits => 472,
            Field::LastTimestamp => std::mem::offset_of!(VoteStateV5, last_timestamp),
        }
    }

    pub fn simd185_field_offset(&self, field: Simd185Field) -> usize {
        match field {
            Simd185Field::InflationRewardsCollector => {
                std::mem::offset_of!(VoteStateV5, inflation_rewards_collector)
            }
            Simd185Field::BlockRevenueCollector => {
                std::mem::offset_of!(VoteStateV5, block_revenue_collector)
            }
            Simd185Field::BlockRevenueCommission => self.block_commission_offset,
            Simd185Field::PendingDelegatorRewards => {
                std::mem::offset_of!(VoteStateV5, pending_delegator_rewards)
            }
            Simd185Field::BlsPubkeyCompressed => {
                std::mem::offset_of!(VoteStateV5, bls_pubkey_compressed)
            }
        }
    }
}
