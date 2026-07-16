//! Dispatch helpers for stake delegation warmup/cooldown math.

use {
    solana_clock::Epoch,
    solana_stake_history::StakeHistoryGetEntry,
    solana_stake_interface::state::{Delegation, Stake, StakeActivationStatus},
};

#[inline]
pub(crate) fn delegation_effective_stake<T: StakeHistoryGetEntry>(
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
pub(crate) fn delegation_activation_status<T: StakeHistoryGetEntry>(
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
pub(crate) fn effective_stake<T: StakeHistoryGetEntry>(
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
