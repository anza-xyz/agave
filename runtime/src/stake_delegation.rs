//! Temporary dispatch helpers for stake delegation math.
//!
//! Selects between legacy floating-point and fixed-point implementations
//! based on a caller-provided feature flag.

use {
    solana_clock::Epoch,
    solana_stake_interface::{
        stake_history::StakeHistoryGetEntry,
        state::{Delegation, Stake, StakeActivationStatus},
    },
};

#[inline]
pub fn delegation_effective<T: StakeHistoryGetEntry>(
    delegation: &Delegation,
    epoch: Epoch,
    history: &T,
    new_rate_activation_epoch: Option<Epoch>,
    use_fixed_point_stake_math: bool,
) -> u64 {
    if use_fixed_point_stake_math {
        delegation.stake_v2(epoch, history, new_rate_activation_epoch)
    } else {
        #[allow(deprecated)]
        delegation.stake(epoch, history, new_rate_activation_epoch)
    }
}

#[inline]
pub fn delegation_status<T: StakeHistoryGetEntry>(
    delegation: &Delegation,
    epoch: Epoch,
    history: &T,
    new_rate_activation_epoch: Option<Epoch>,
    use_fixed_point_stake_math: bool,
) -> StakeActivationStatus {
    if use_fixed_point_stake_math {
        delegation.stake_activating_and_deactivating_v2(epoch, history, new_rate_activation_epoch)
    } else {
        #[allow(deprecated)]
        delegation.stake_activating_and_deactivating(epoch, history, new_rate_activation_epoch)
    }
}

#[inline]
pub fn stake_effective<T: StakeHistoryGetEntry>(
    stake: &Stake,
    epoch: Epoch,
    history: &T,
    new_rate_activation_epoch: Option<Epoch>,
    use_fixed_point_stake_math: bool,
) -> u64 {
    if use_fixed_point_stake_math {
        stake.stake_v2(epoch, history, new_rate_activation_epoch)
    } else {
        #[allow(deprecated)]
        stake.stake(epoch, history, new_rate_activation_epoch)
    }
}
