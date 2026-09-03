//! Helpers for stake delegation math.

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
pub(crate) fn is_delegation_inert<T: StakeHistoryGetEntry>(
    delegation: &Delegation,
    epoch: Epoch,
    history: &T,
    new_rate_activation_epoch: Option<Epoch>,
    use_fixed_point_stake_math: bool,
) -> bool {
    let activation_status = delegation_activation_status(
        delegation,
        epoch,
        history,
        new_rate_activation_epoch,
        use_fixed_point_stake_math,
    );
    activation_status.effective == 0 && activation_status.activating == 0
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
