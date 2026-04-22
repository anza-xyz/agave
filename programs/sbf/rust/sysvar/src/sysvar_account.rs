#[allow(deprecated)]
use solana_sysvar::recent_blockhashes::RecentBlockhashes;
use {
    serde::de::DeserializeOwned,
    solana_account_info::AccountInfo,
    solana_program_error::ProgramError,
    solana_stake_interface::stake_history::StakeHistory,
    solana_sysvar::{
        clock::Clock, epoch_rewards::EpochRewards, epoch_schedule::EpochSchedule, rent::Rent,
        slot_hashes::SlotHashes, slot_history::SlotHistory,
    },
    solana_sysvar_id::SysvarId,
};

pub(crate) trait SysvarAccountDeserialize: DeserializeOwned + SysvarId {
    const IS_SUPPORTED: bool = true;
}

impl SysvarAccountDeserialize for Clock {}
impl SysvarAccountDeserialize for EpochRewards {}
impl SysvarAccountDeserialize for EpochSchedule {}
impl SysvarAccountDeserialize for Rent {}
#[allow(deprecated)]
impl SysvarAccountDeserialize for RecentBlockhashes {}
impl SysvarAccountDeserialize for StakeHistory {}
impl SysvarAccountDeserialize for SlotHashes {
    const IS_SUPPORTED: bool = false;
}
impl SysvarAccountDeserialize for SlotHistory {
    const IS_SUPPORTED: bool = false;
}

pub(crate) fn deserialize_sysvar_account<T: SysvarAccountDeserialize>(
    account: &AccountInfo,
) -> Result<T, ProgramError> {
    if !T::IS_SUPPORTED {
        return Err(ProgramError::UnsupportedSysvar);
    }
    if !T::check_id(account.unsigned_key()) {
        return Err(ProgramError::InvalidArgument);
    }
    bincode::deserialize(&account.data.borrow()).map_err(|_| ProgramError::InvalidArgument)
}
