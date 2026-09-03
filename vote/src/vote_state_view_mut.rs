use {
    crate::vote_state_view::{VoteStateViewError, frame_v4::VoteStateFrameV4},
    core::fmt::Debug,
    solana_vote_interface::state::VoteStateV4,
};

/// A view into a serialized VoteState.
///
/// This struct provides access to the VoteState data without
/// deserializing it. This is done by parsing and caching metadata
/// about the layout of the serialized VoteState.
#[derive(Debug)]
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
pub struct VoteStateViewMut<'account> {
    data: &'account mut [u8],
}

const VOTE_STATE_V4_DISCRIMINANT: u32 = 3;
const PENDING_DELEGATOR_REWARDS_START_OFFSET: usize =
    VoteStateFrameV4::pending_delegator_rewards_offset();
const PENDING_DELEGATOR_REWARDS_END_OFFSET: usize =
    VoteStateFrameV4::pending_delegator_rewards_offset() + core::mem::size_of::<u64>();

// Just to make sure the size check will always work in the constructor
const _: () = assert!(VoteStateV4::size_of() > core::mem::size_of::<u32>());
// Just to make sure the pending delegator rewards will always be available
const _: () = assert!(VoteStateV4::size_of() > PENDING_DELEGATOR_REWARDS_END_OFFSET);

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
        Ok(Self { data })
    }

    /// Increment pending delegator rewards, used during block reward deposit
    pub fn increment_pending_delegator_rewards_checked(&mut self, lamports: u64) -> Option<()> {
        // Size is checked at creation, so this is safe
        let current_pending_delegator_rewards = u64::from_le_bytes(
            self.data[PENDING_DELEGATOR_REWARDS_START_OFFSET..PENDING_DELEGATOR_REWARDS_END_OFFSET]
                .try_into()
                .unwrap(),
        );
        let new_pending_delegator_rewards =
            current_pending_delegator_rewards.checked_add(lamports)?;
        self.data[PENDING_DELEGATOR_REWARDS_START_OFFSET..PENDING_DELEGATOR_REWARDS_END_OFFSET]
            .copy_from_slice(&new_pending_delegator_rewards.to_le_bytes());
        Some(())
    }

    /// Reset pending delegator rewards, used during vote account sweep at epoch
    /// rollover
    pub fn reset_pending_delegator_rewards(&mut self) -> u64 {
        // Size is checked at creation, so this is safe
        let pending_delegator_rewards = u64::from_le_bytes(
            self.data[PENDING_DELEGATOR_REWARDS_START_OFFSET..PENDING_DELEGATOR_REWARDS_END_OFFSET]
                .try_into()
                .unwrap(),
        );
        self.data[PENDING_DELEGATOR_REWARDS_START_OFFSET..PENDING_DELEGATOR_REWARDS_END_OFFSET]
            .copy_from_slice(&0u64.to_le_bytes());
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
}
