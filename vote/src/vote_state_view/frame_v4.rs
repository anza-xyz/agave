use {
    super::{
        AuthorizedVotersListFrame, EpochCreditsListFrame, Field, Result, RootSlotFrame,
        Simd185Field, VoteStateViewError,
        field_frames::{BlsPubkeyCompressedFrame, LandedVotesListFrame, ListFrame},
    },
    solana_pubkey::Pubkey,
    solana_vote_interface::state::BlockTimestamp,
    std::io::BufRead,
};

#[derive(Debug, PartialEq, Clone)]
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
pub(crate) struct VoteStateFrameV4 {
    pub(super) bls_pubkey_compressed_frame: BlsPubkeyCompressedFrame,
    pub(super) votes_frame: LandedVotesListFrame,
    pub(super) root_slot_frame: RootSlotFrame,
    pub(super) authorized_voters_frame: AuthorizedVotersListFrame,
    pub(super) epoch_credits_frame: EpochCreditsListFrame,
}

impl VoteStateFrameV4 {
    pub(crate) fn try_new(bytes: &[u8]) -> Result<Self> {
        let bls_pubkey_offset = Self::bls_pubkey_compressed_offset();
        let mut cursor = std::io::Cursor::new(bytes);
        cursor.set_position(bls_pubkey_offset as u64);

        let bls_pubkey_compressed_frame = BlsPubkeyCompressedFrame::read(&mut cursor)?;
        let votes_frame = LandedVotesListFrame::read(&mut cursor)?;
        let root_slot_frame = RootSlotFrame::read(&mut cursor)?;
        let authorized_voters_frame = AuthorizedVotersListFrame::read(&mut cursor)?;
        let epoch_credits_frame = EpochCreditsListFrame::read(&mut cursor)?;
        cursor.consume(core::mem::size_of::<BlockTimestamp>());
        if cursor.position() as usize <= bytes.len() {
            Ok(Self {
                bls_pubkey_compressed_frame,
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
            Field::Commission => Self::inflation_rewards_commission_offset(),
            Field::Votes => self.votes_offset(),
            Field::RootSlot => self.root_slot_offset(),
            Field::AuthorizedVoters => self.authorized_voters_offset(),
            Field::EpochCredits => self.epoch_credits_offset(),
            Field::LastTimestamp => self.last_timestamp_offset(),
        }
    }

    pub(super) fn simd185_field_offset(&self, field: Simd185Field) -> usize {
        match field {
            Simd185Field::InflationRewardsCollector => Self::inflation_rewards_collector_offset(),
            Simd185Field::BlockRevenueCollector => Self::block_revenue_collector_offset(),
            Simd185Field::BlockRevenueCommission => Self::block_revenue_commission_offset(),
            Simd185Field::PendingDelegatorRewards => Self::pending_delegator_rewards_offset(),
            Simd185Field::BlsPubkeyCompressed => Self::bls_pubkey_compressed_offset(),
        }
    }

    const fn node_pubkey_offset() -> usize {
        core::mem::size_of::<u32>() // version
    }

    const fn authorized_withdrawer_offset() -> usize {
        Self::node_pubkey_offset() + core::mem::size_of::<Pubkey>()
    }

    const fn inflation_rewards_collector_offset() -> usize {
        Self::authorized_withdrawer_offset() + core::mem::size_of::<Pubkey>()
    }

    const fn block_revenue_collector_offset() -> usize {
        Self::inflation_rewards_collector_offset() + core::mem::size_of::<Pubkey>()
    }

    const fn inflation_rewards_commission_offset() -> usize {
        Self::block_revenue_collector_offset() + core::mem::size_of::<Pubkey>()
    }

    const fn block_revenue_commission_offset() -> usize {
        Self::inflation_rewards_commission_offset() + core::mem::size_of::<u16>()
    }

    const fn pending_delegator_rewards_offset() -> usize {
        Self::block_revenue_commission_offset() + core::mem::size_of::<u16>()
    }

    const fn bls_pubkey_compressed_offset() -> usize {
        Self::pending_delegator_rewards_offset() + core::mem::size_of::<u64>()
    }

    fn votes_offset(&self) -> usize {
        Self::bls_pubkey_compressed_offset() + self.bls_pubkey_compressed_frame.total_size()
    }

    fn root_slot_offset(&self) -> usize {
        self.votes_offset() + self.votes_frame.total_size()
    }

    fn authorized_voters_offset(&self) -> usize {
        self.root_slot_offset() + self.root_slot_frame.total_size()
    }

    fn epoch_credits_offset(&self) -> usize {
        self.authorized_voters_offset() + self.authorized_voters_frame.total_size()
    }

    fn last_timestamp_offset(&self) -> usize {
        self.epoch_credits_offset() + self.epoch_credits_frame.total_size()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        serde::{Deserialize, Serialize},
        solana_vote_interface::{
            authorized_voters::AuthorizedVoters,
            state::{BLS_PUBLIC_KEY_COMPRESSED_SIZE, LandedVote, Lockout, VoteStateV4},
        },
        std::collections::VecDeque,
    };

    #[allow(clippy::large_enum_variant)]
    #[derive(Debug, Clone, Deserialize, Serialize)]
    enum TestVoteStateVersions {
        V0_23_5,
        V1_14_11,
        V3,
        V4(VoteStateV4),
    }

    #[test]
    fn test_try_new_zeroed() {
        let target_vote_state = VoteStateV4::default();
        let target_vote_state_versions = TestVoteStateVersions::V4(target_vote_state);
        let mut bytes = bincode::serialize(&target_vote_state_versions).unwrap();

        for i in 0..bytes.len() {
            let vote_state_frame = VoteStateFrameV4::try_new(&bytes[..i]);
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
                VoteStateFrameV4::try_new(&bytes),
                Ok(VoteStateFrameV4 {
                    bls_pubkey_compressed_frame: BlsPubkeyCompressedFrame { has_pubkey: false },
                    votes_frame: LandedVotesListFrame { len: 0 },
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
        let target_vote_state = VoteStateV4 {
            authorized_voters: AuthorizedVoters::new(0, Pubkey::default()),
            epoch_credits: vec![(1, 2, 3)],
            bls_pubkey_compressed: Some([42; BLS_PUBLIC_KEY_COMPRESSED_SIZE]),
            votes: VecDeque::from([LandedVote {
                latency: 0,
                lockout: Lockout::default(),
            }]),
            root_slot: Some(42),
            ..VoteStateV4::default()
        };

        let target_vote_state_versions = TestVoteStateVersions::V4(target_vote_state);
        let mut bytes = bincode::serialize(&target_vote_state_versions).unwrap();

        for i in 0..bytes.len() {
            let vote_state_frame = VoteStateFrameV4::try_new(&bytes[..i]);
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
                VoteStateFrameV4::try_new(&bytes),
                Ok(VoteStateFrameV4 {
                    bls_pubkey_compressed_frame: BlsPubkeyCompressedFrame { has_pubkey: true },
                    votes_frame: LandedVotesListFrame { len: 1 },
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
        let mut bytes = vec![0; VoteStateFrameV4::bls_pubkey_compressed_offset()];

        {
            let mut bytes = bytes.clone();
            bytes.extend_from_slice(&(2u8.to_le_bytes()));
            let vote_state_frame = VoteStateFrameV4::try_new(&bytes);
            assert_eq!(
                vote_state_frame,
                Err(VoteStateViewError::InvalidBlsPubkeyCompressedOption)
            );
        }

        bytes.extend_from_slice(&[0; 1]);

        {
            let mut bytes = bytes.clone();
            bytes.extend_from_slice(&(256u64.to_le_bytes()));
            let vote_state_frame = VoteStateFrameV4::try_new(&bytes);
            assert_eq!(
                vote_state_frame,
                Err(VoteStateViewError::InvalidVotesLength)
            );
        }

        bytes.extend_from_slice(&[0; core::mem::size_of::<u64>()]);

        {
            let mut bytes = bytes.clone();
            bytes.extend_from_slice(&(2u8.to_le_bytes()));
            let vote_state_frame = VoteStateFrameV4::try_new(&bytes);
            assert_eq!(
                vote_state_frame,
                Err(VoteStateViewError::InvalidRootSlotOption)
            );
        }

        bytes.extend_from_slice(&[0; 1]);

        {
            let mut bytes = bytes.clone();
            bytes.extend_from_slice(&(256u64.to_le_bytes()));
            let vote_state_frame = VoteStateFrameV4::try_new(&bytes);
            assert_eq!(
                vote_state_frame,
                Err(VoteStateViewError::InvalidAuthorizedVotersLength)
            );
        }

        bytes.extend_from_slice(&[0; core::mem::size_of::<u64>()]);

        {
            let mut bytes = bytes.clone();
            bytes.extend_from_slice(&(256u64.to_le_bytes()));
            let vote_state_frame = VoteStateFrameV4::try_new(&bytes);
            assert_eq!(
                vote_state_frame,
                Err(VoteStateViewError::InvalidEpochCreditsLength)
            );
        }
    }
}
