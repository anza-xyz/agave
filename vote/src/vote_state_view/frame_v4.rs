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
        solana_vote_interface::{
            authorized_voters::AuthorizedVoters,
            state::{
                BLS_PUBLIC_KEY_COMPRESSED_SIZE, LandedVote, Lockout, VoteStateV4, VoteStateVersions,
            },
        },
        std::collections::VecDeque,
    };

    #[test]
    fn test_try_new_zeroed() {
        let target_vote_state = VoteStateV4::default();
        let versioned = VoteStateVersions::new_v4(target_vote_state);
        let mut bytes = bincode::serialize(&versioned).unwrap();

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

        let versioned = VoteStateVersions::new_v4(target_vote_state);
        let mut bytes = bincode::serialize(&versioned).unwrap();

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

    #[test]
    fn test_try_new_trailing_nonzero_bytes() {
        let vote_state = VoteStateV4 {
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
        let versioned = VoteStateVersions::new_v4(vote_state);
        let mut bytes = bincode::serialize(&versioned).unwrap();

        // Append non-zero trailing garbage.
        bytes.extend_from_slice(&[0xFF; 42]);

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

    #[test]
    fn test_frame_v4_field_offsets_match_sdk() {
        // Verify frame offset calculations produce correct values by
        // serializing a known VoteStateV4 and reading at computed offsets.
        let node_pubkey = Pubkey::from([1; 32]);
        let authorized_withdrawer = Pubkey::from([2; 32]);
        let inflation_rewards_commission_bps = 5_000;
        let block_revenue_commission_bps = 7_500;
        let inflation_rewards_collector = Pubkey::from([3; 32]);
        let block_revenue_collector = Pubkey::from([4; 32]);
        let pending_delegator_rewards = 42;
        let bls_pubkey_compressed = [42u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE];
        let authorized_voter = Pubkey::from([5; 32]);
        let root_slot = 17u64;
        let last_timestamp = BlockTimestamp {
            slot: 100,
            timestamp: 1_700_000_000,
        };
        let lockout = Lockout::new_with_confirmation_count(42, 3);
        let landed_vote = LandedVote {
            latency: 7,
            lockout,
        };
        let epoch_credits = vec![(9u64, 100u64, 50u64)];
        let vote_state = VoteStateV4 {
            node_pubkey,
            authorized_withdrawer,
            inflation_rewards_commission_bps,
            block_revenue_commission_bps,
            inflation_rewards_collector,
            block_revenue_collector,
            pending_delegator_rewards,
            bls_pubkey_compressed: Some(bls_pubkey_compressed),
            votes: VecDeque::from([landed_vote]),
            root_slot: Some(root_slot),
            authorized_voters: AuthorizedVoters::new(0, authorized_voter),
            epoch_credits: epoch_credits.clone(),
            last_timestamp: last_timestamp.clone(),
        };
        let versioned = VoteStateVersions::new_v4(vote_state);
        let bytes = bincode::serialize(&versioned).unwrap();
        let frame = VoteStateFrameV4::try_new(&bytes).unwrap();

        // node_pubkey
        let offset = VoteStateFrameV4::node_pubkey_offset();
        assert_eq!(&bytes[offset..offset + 32], node_pubkey.as_ref());

        // authorized_withdrawer
        let offset = VoteStateFrameV4::authorized_withdrawer_offset();
        assert_eq!(&bytes[offset..offset + 32], authorized_withdrawer.as_ref());

        // inflation_rewards_collector
        let offset = VoteStateFrameV4::inflation_rewards_collector_offset();
        assert_eq!(
            &bytes[offset..offset + 32],
            inflation_rewards_collector.as_ref()
        );

        // block_revenue_collector
        let offset = VoteStateFrameV4::block_revenue_collector_offset();
        assert_eq!(
            &bytes[offset..offset + 32],
            block_revenue_collector.as_ref()
        );

        // inflation_rewards_commission
        let offset = VoteStateFrameV4::inflation_rewards_commission_offset();
        assert_eq!(
            u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap()),
            inflation_rewards_commission_bps
        );

        // block_revenue_commission
        let offset = VoteStateFrameV4::block_revenue_commission_offset();
        assert_eq!(
            u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap()),
            block_revenue_commission_bps
        );

        // pending_delegator_rewards
        let offset = VoteStateFrameV4::pending_delegator_rewards_offset();
        assert_eq!(
            u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap()),
            pending_delegator_rewards
        );

        // bls_pubkey_compressed
        let offset = VoteStateFrameV4::bls_pubkey_compressed_offset();
        assert_eq!(bytes[offset], 1);
        assert_eq!(
            &bytes[offset + 1..offset + 1 + BLS_PUBLIC_KEY_COMPRESSED_SIZE],
            &bls_pubkey_compressed,
        );

        // votes: 8-byte length prefix + one LandedVote
        let offset = frame.votes_offset();
        assert_eq!(
            u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap()),
            1
        );
        let item_offset = offset + 8;
        assert_eq!(bytes[item_offset], landed_vote.latency);
        assert_eq!(
            u64::from_le_bytes(bytes[item_offset + 1..item_offset + 9].try_into().unwrap()),
            lockout.slot()
        );
        assert_eq!(
            u32::from_le_bytes(bytes[item_offset + 9..item_offset + 13].try_into().unwrap()),
            lockout.confirmation_count()
        );

        // root_slot
        let offset = frame.root_slot_offset();
        assert_eq!(bytes[offset], 1);
        assert_eq!(
            u64::from_le_bytes(bytes[offset + 1..offset + 9].try_into().unwrap()),
            root_slot
        );

        // authorized_voters: 8-byte length + one entry
        let offset = frame.authorized_voters_offset();
        assert_eq!(
            u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap()),
            1
        );
        let entry_offset = offset + 8;
        assert_eq!(
            u64::from_le_bytes(bytes[entry_offset..entry_offset + 8].try_into().unwrap()),
            0
        );
        assert_eq!(
            &bytes[entry_offset + 8..entry_offset + 8 + 32],
            authorized_voter.as_ref()
        );

        // epoch_credits: 8-byte length + one entry
        let offset = frame.epoch_credits_offset();
        assert_eq!(
            u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap()),
            1
        );
        let (expected_epoch, expected_credits, expected_prev) = epoch_credits[0];
        let entry_offset = offset + 8;
        assert_eq!(
            u64::from_le_bytes(bytes[entry_offset..entry_offset + 8].try_into().unwrap()),
            expected_epoch
        );
        assert_eq!(
            u64::from_le_bytes(
                bytes[entry_offset + 8..entry_offset + 16]
                    .try_into()
                    .unwrap()
            ),
            expected_credits
        );
        assert_eq!(
            u64::from_le_bytes(
                bytes[entry_offset + 16..entry_offset + 24]
                    .try_into()
                    .unwrap()
            ),
            expected_prev
        );

        // last_timestamp
        let offset = frame.last_timestamp_offset();
        assert_eq!(
            u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap()),
            last_timestamp.slot
        );
        assert_eq!(
            i64::from_le_bytes(bytes[offset + 8..offset + 16].try_into().unwrap()),
            last_timestamp.timestamp
        );
    }
}
