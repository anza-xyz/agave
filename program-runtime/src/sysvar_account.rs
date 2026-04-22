//! Helpers for creating, reading, and writing sysvar accounts.
//! Replaces `solana_account`'s `SysvarSerialize`-bound helpers.

#[allow(deprecated)]
use solana_sysvar::{fees::Fees, recent_blockhashes::RecentBlockhashes};
use {
    bincode::serialized_size,
    serde::{Serialize, de::DeserializeOwned},
    solana_account::{
        Account, AccountSharedData, InheritableAccountFields, ReadableAccount, WritableAccount,
    },
    solana_clock::Clock,
    solana_epoch_rewards::EpochRewards,
    solana_epoch_schedule::EpochSchedule,
    solana_last_restart_slot::LastRestartSlot,
    solana_rent::Rent,
    solana_sdk_ids::sysvar,
    solana_slot_hashes::SlotHashes,
    solana_stake_interface::stake_history::StakeHistory,
    solana_sysvar::{rewards::Rewards, slot_history::SlotHistory},
    solana_sysvar_id::SysvarId,
};

pub trait SysvarAccountSize: Serialize + SysvarId {
    const MIN_ACCOUNT_DATA_LEN: usize = 0;
}

impl SysvarAccountSize for Clock {}
impl SysvarAccountSize for EpochRewards {}
impl SysvarAccountSize for EpochSchedule {}
impl SysvarAccountSize for LastRestartSlot {}
impl SysvarAccountSize for Rent {}
impl SysvarAccountSize for Rewards {}

#[allow(deprecated)]
impl SysvarAccountSize for Fees {}
#[allow(deprecated)]
impl SysvarAccountSize for RecentBlockhashes {
    // https://github.com/anza-xyz/solana-sdk/blob/aa234003529fae950aec9550c99ee25033a529a1/sysvar/src/recent_blockhashes.rs#L157-L160
    const MIN_ACCOUNT_DATA_LEN: usize = 6008;
}
impl SysvarAccountSize for SlotHashes {
    // https://github.com/anza-xyz/solana-sdk/blob/aa234003529fae950aec9550c99ee25033a529a1/sysvar/src/slot_hashes.rs#L56-L57
    const MIN_ACCOUNT_DATA_LEN: usize = 20_488;
}
impl SysvarAccountSize for SlotHistory {
    // https://github.com/anza-xyz/solana-sdk/blob/aa234003529fae950aec9550c99ee25033a529a1/sysvar/src/slot_history.rs#L61-L65
    const MIN_ACCOUNT_DATA_LEN: usize = 131_097;
}
impl SysvarAccountSize for StakeHistory {
    // https://github.com/solana-program/stake/blob/3fc55bf917ef1dd4693318844f2bb696eb3485cd/interface/src/sysvar/stake_history.rs#L59-L64
    const MIN_ACCOUNT_DATA_LEN: usize = 16_392;
}

pub fn sysvar_account_data_len<T: SysvarAccountSize>(sysvar: &T) -> usize {
    let serialized_len = serialized_size(sysvar).unwrap() as usize;
    serialized_len.max(T::MIN_ACCOUNT_DATA_LEN)
}

pub fn create_account_with_fields<T: SysvarAccountSize>(
    sysvar_data: &T,
    (lamports, rent_epoch): InheritableAccountFields,
) -> Account {
    let data_len = sysvar_account_data_len(sysvar_data);
    let mut account = Account {
        lamports,
        data: vec![0; data_len],
        owner: sysvar::id(),
        executable: false,
        rent_epoch,
    };
    to_account(sysvar_data, &mut account).unwrap();
    account
}

pub fn create_account_for_test<T: SysvarAccountSize>(sysvar_data: &T) -> Account {
    create_account_with_fields(sysvar_data, (1, solana_clock::INITIAL_RENT_EPOCH))
}

pub fn create_account_shared_data_with_fields<T: SysvarAccountSize>(
    sysvar_data: &T,
    fields: InheritableAccountFields,
) -> AccountSharedData {
    AccountSharedData::from(create_account_with_fields(sysvar_data, fields))
}

pub fn create_account_shared_data_for_test<T: SysvarAccountSize>(
    sysvar_data: &T,
) -> AccountSharedData {
    AccountSharedData::from(create_account_for_test(sysvar_data))
}

pub fn from_account<T: DeserializeOwned, U: ReadableAccount>(account: &U) -> Option<T> {
    bincode::deserialize(account.data()).ok()
}

pub fn to_account<T: Serialize, U: WritableAccount>(
    sysvar_data: &T,
    account: &mut U,
) -> Option<()> {
    if serialized_size(sysvar_data).ok()? > account.data().len() as u64 {
        return None;
    }
    bincode::serialize_into(account.data_as_mut_slice(), sysvar_data).ok()
}
