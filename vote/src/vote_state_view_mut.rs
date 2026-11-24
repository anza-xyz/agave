use {
    crate::vote_state_view::{VoteStateViewError, frame_v4::VoteStateFrameV4},
    core::fmt::Debug,
    solana_vote_interface::state::{
        VoteStateV4,
        vote_state_v5::{VOTE_STATE_V5_DISCRIMINANT, VOTE_STATE_V5_SIZE, VoteStateV5},
    },
};

/// A view into a serialized VoteState.
///
/// This struct provides access to the VoteState data without
/// deserializing it. It reads the fields it needs at fixed offsets, so the
/// data needs no particular alignment.
#[derive(Debug)]
pub struct VoteStateViewMut<'account> {
    data: &'account mut [u8],
    pending_delegator_rewards_offset: usize,
}

const VOTE_STATE_V4_DISCRIMINANT: u32 = 3;
const PENDING_DELEGATOR_REWARDS_V4_OFFSET: usize =
    VoteStateFrameV4::pending_delegator_rewards_offset();
const PENDING_DELEGATOR_REWARDS_V5_OFFSET: usize =
    core::mem::offset_of!(VoteStateV5, pending_delegator_rewards);
const PENDING_DELEGATOR_REWARDS_SIZE: usize = core::mem::size_of::<u64>();

// Just to make sure the size check will always work in the constructor
const _: () = assert!(VoteStateV4::size_of() > core::mem::size_of::<u32>());
// Just to make sure the pending delegator rewards will always be available
const _: () = assert!(
    VoteStateV4::size_of() > PENDING_DELEGATOR_REWARDS_V4_OFFSET + PENDING_DELEGATOR_REWARDS_SIZE
);
const _: () = assert!(
    VOTE_STATE_V5_SIZE > PENDING_DELEGATOR_REWARDS_V5_OFFSET + PENDING_DELEGATOR_REWARDS_SIZE
);

impl<'account> VoteStateViewMut<'account> {
    /// Strict constructor that only works with the latest `VoteStateV4`
    pub fn new_v4(data: &'account mut [u8]) -> Result<Self, VoteStateViewError> {
        if data.len() < VoteStateV4::size_of() {
            return Err(VoteStateViewError::AccountDataSizeIncorrect);
        }
        // Size was just checked, so this is safe
        let version = u32::from_le_bytes(data[0..4].try_into().unwrap());
        if version != VOTE_STATE_V4_DISCRIMINANT {
            return Err(VoteStateViewError::UnsupportedVersion);
        }
        Ok(Self {
            data,
            pending_delegator_rewards_offset: PENDING_DELEGATOR_REWARDS_V4_OFFSET,
        })
    }

    /// Strict constructor that only works with a valid `VoteStateV5`
    pub fn new_v5(data: &'account mut [u8]) -> Result<Self, VoteStateViewError> {
        VoteStateV5::from_account_data_unaligned(data)
            .map_err(|_| VoteStateViewError::InvalidVoteStateV5)?;
        Ok(Self {
            data,
            pending_delegator_rewards_offset: PENDING_DELEGATOR_REWARDS_V5_OFFSET,
        })
    }

    /// Constructor for the latest versions, `VoteStateV4` and `VoteStateV5`
    pub fn new(data: &'account mut [u8]) -> Result<Self, VoteStateViewError> {
        match data
            .get(..4)
            .map(|tag| u32::from_le_bytes(tag.try_into().unwrap()))
        {
            Some(VOTE_STATE_V5_DISCRIMINANT) => Self::new_v5(data),
            _ => Self::new_v4(data),
        }
    }

    fn pending_delegator_rewards_range(&self) -> core::ops::Range<usize> {
        self.pending_delegator_rewards_offset
            ..self.pending_delegator_rewards_offset + PENDING_DELEGATOR_REWARDS_SIZE
    }

    /// Increment pending delegator rewards, used during block reward deposit
    pub fn increment_pending_delegator_rewards_checked(&mut self, lamports: u64) -> Option<()> {
        let range = self.pending_delegator_rewards_range();
        // Size is checked at creation, so this is safe
        let current_pending_delegator_rewards =
            u64::from_le_bytes(self.data[range.clone()].try_into().unwrap());
        let new_pending_delegator_rewards =
            current_pending_delegator_rewards.checked_add(lamports)?;
        self.data[range].copy_from_slice(&new_pending_delegator_rewards.to_le_bytes());
        Some(())
    }

    /// Reset pending delegator rewards, used during vote account sweep at epoch
    /// rollover
    pub fn reset_pending_delegator_rewards(&mut self) -> u64 {
        let range = self.pending_delegator_rewards_range();
        // Size is checked at creation, so this is safe
        let pending_delegator_rewards =
            u64::from_le_bytes(self.data[range.clone()].try_into().unwrap());
        self.data[range].copy_from_slice(&0u64.to_le_bytes());
        pending_delegator_rewards
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_vote_interface::state::VoteStateVersions};

    #[test]
    fn match_vote_state_v4_discriminant() {
        let target_vote_state = VoteStateV4::default();
        let versioned = VoteStateVersions::new_v4(target_vote_state);
        let mut bytes = vec![0; VoteStateV4::size_of()];
        VoteStateV4::serialize(&versioned, &mut bytes).unwrap();
        assert_eq!(bytes[0..4], VOTE_STATE_V4_DISCRIMINANT.to_le_bytes());
    }

    #[test]
    fn success_new_v4() {
        let target_vote_state = VoteStateV4::default();
        let versioned = VoteStateVersions::new_v4(target_vote_state);
        let mut bytes = vec![0; VoteStateV4::size_of()];
        VoteStateV4::serialize(&versioned, &mut bytes).unwrap();
        VoteStateViewMut::new_v4(&mut bytes).unwrap();

        // Too big still works
        bytes.resize(VoteStateV4::size_of() + 1, 0);
        VoteStateViewMut::new_v4(&mut bytes).unwrap();
    }

    #[test]
    fn fail_new_v4() {
        let target_vote_state = VoteStateV4::default();
        let versioned = VoteStateVersions::new_v4(target_vote_state);
        let mut bytes = vec![0; VoteStateV4::size_of()];
        VoteStateV4::serialize(&versioned, &mut bytes).unwrap();

        // Wrong discriminant
        bytes[0] = 0;
        assert_eq!(
            VoteStateViewMut::new_v4(&mut bytes).unwrap_err(),
            VoteStateViewError::UnsupportedVersion,
        );

        // Too small
        bytes.resize(VoteStateV4::size_of() - 1, 0);
        assert_eq!(
            VoteStateViewMut::new_v4(&mut bytes).unwrap_err(),
            VoteStateViewError::AccountDataSizeIncorrect,
        );
    }

    #[test]
    fn increment_pending_delegator_rewards() {
        let target_vote_state = VoteStateV4::default();
        let versioned = VoteStateVersions::new_v4(target_vote_state);
        let mut bytes = vec![0; VoteStateV4::size_of()];
        VoteStateV4::serialize(&versioned, &mut bytes).unwrap();

        let mut view = VoteStateViewMut::new_v4(&mut bytes).unwrap();
        view.increment_pending_delegator_rewards_checked(1).unwrap();

        let vote_state = VoteStateVersions::deserialize(&bytes).unwrap();
        let VoteStateVersions::V4(v4) = vote_state else {
            panic!("Not vote state v4");
        };
        assert_eq!(v4.pending_delegator_rewards, 1);

        let mut view = VoteStateViewMut::new_v4(&mut bytes).unwrap();
        view.increment_pending_delegator_rewards_checked(u64::MAX - 1)
            .unwrap();

        let vote_state = VoteStateVersions::deserialize(&bytes).unwrap();
        let VoteStateVersions::V4(v4) = vote_state else {
            panic!("Not vote state v4");
        };
        assert_eq!(v4.pending_delegator_rewards, u64::MAX);

        let mut view = VoteStateViewMut::new_v4(&mut bytes).unwrap();
        assert!(
            view.increment_pending_delegator_rewards_checked(1)
                .is_none()
        );
    }

    #[test]
    fn reset_pending_delegator_rewards() {
        let target_vote_state = VoteStateV4::default();
        let versioned = VoteStateVersions::new_v4(target_vote_state);
        let mut bytes = vec![0; VoteStateV4::size_of()];
        VoteStateV4::serialize(&versioned, &mut bytes).unwrap();

        let expected_pending_delegator_rewards = u64::MAX;
        let mut view = VoteStateViewMut::new_v4(&mut bytes).unwrap();
        view.increment_pending_delegator_rewards_checked(expected_pending_delegator_rewards)
            .unwrap();

        let pending_delegator_rewards = view.reset_pending_delegator_rewards();
        assert_eq!(
            expected_pending_delegator_rewards,
            pending_delegator_rewards
        );

        let vote_state = VoteStateVersions::deserialize(&bytes).unwrap();
        let VoteStateVersions::V4(v4) = vote_state else {
            panic!("Not vote state v4");
        };
        assert_eq!(v4.pending_delegator_rewards, 0);
    }

    #[test]
    fn v5_increment_and_reset_pending_delegator_rewards() {
        use solana_vote_interface::state::vote_state_v5::{Location, VOTE_STATE_V5_SIZE};
        let mut state = VoteStateV5::from_vote_state_v4(
            VoteStateV4 {
                bls_pubkey_compressed: Some([1; 48]),
                pending_delegator_rewards: 7,
                ..Default::default()
            },
            Location::try_new(4051542, 618526, 4870645).unwrap(),
            3,
        )
        .unwrap();
        state.pending_delegator_rewards = 7;
        let mut bytes = vec![0; VOTE_STATE_V5_SIZE];
        state.write_account_data(&mut bytes).unwrap();

        let mut view = VoteStateViewMut::new(&mut bytes).unwrap();
        assert_eq!(
            view.increment_pending_delegator_rewards_checked(5),
            Some(())
        );
        assert_eq!(
            view.increment_pending_delegator_rewards_checked(u64::MAX),
            None
        );
        assert_eq!(view.reset_pending_delegator_rewards(), 12);
        assert_eq!(view.reset_pending_delegator_rewards(), 0);
        // the account is still a valid v5 account
        assert!(VoteStateV5::from_account_data_ref(&bytes).is_ok());
        // size must be exact
        bytes.push(0);
        assert!(VoteStateViewMut::new(&mut bytes).is_err());
    }

    #[test]
    fn v5_pending_delegator_rewards_at_any_alignment() {
        use solana_vote_interface::state::vote_state_v5::Location;
        let state = VoteStateV5::from_vote_state_v4(
            VoteStateV4 {
                bls_pubkey_compressed: Some([1; 48]),
                pending_delegator_rewards: 7,
                ..Default::default()
            },
            Location::try_new(4051542, 618526, 4870645).unwrap(),
            3,
        )
        .unwrap();
        let mut storage = vec![0xff; VOTE_STATE_V5_SIZE + 8];
        for offset in 0..8 {
            let bytes = &mut storage[offset..offset + VOTE_STATE_V5_SIZE];
            state.write_account_data(bytes).unwrap();
            let mut view = VoteStateViewMut::new(bytes).unwrap();
            assert_eq!(
                view.increment_pending_delegator_rewards_checked(5),
                Some(())
            );
            assert_eq!(view.reset_pending_delegator_rewards(), 12);
            assert_eq!(
                VoteStateV5::from_account_data_unaligned(bytes)
                    .unwrap()
                    .pending_delegator_rewards,
                0
            );
        }
    }
}
