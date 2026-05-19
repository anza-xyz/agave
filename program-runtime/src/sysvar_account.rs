//! Helpers for creating, reading, and writing sysvar accounts.
//! Replaces `solana_account`'s `SysvarSerialize`-bound helpers.

#[allow(deprecated)]
use solana_sysvar::{fees::Fees, recent_blockhashes::RecentBlockhashes};
use {
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

pub trait SysvarAccountSize: SysvarId {
    const SIZE: usize;
}

impl SysvarAccountSize for Clock {
    const SIZE: usize = 40;
}

impl SysvarAccountSize for EpochRewards {
    const SIZE: usize = 81;
}

impl SysvarAccountSize for EpochSchedule {
    const SIZE: usize = 33;
}

impl SysvarAccountSize for LastRestartSlot {
    const SIZE: usize = 8;
}

impl SysvarAccountSize for Rent {
    const SIZE: usize = 17;
}

impl SysvarAccountSize for Rewards {
    const SIZE: usize = 16;
}

#[allow(deprecated)]
impl SysvarAccountSize for Fees {
    const SIZE: usize = 8;
}

#[allow(deprecated)]
impl SysvarAccountSize for RecentBlockhashes {
    const SIZE: usize = 6008;
}

impl SysvarAccountSize for SlotHashes {
    const SIZE: usize = 20_488;
}

impl SysvarAccountSize for SlotHistory {
    const SIZE: usize = 131_097;
}

impl SysvarAccountSize for StakeHistory {
    const SIZE: usize = 16_392;
}

fn create_account_with_fields<T: Serialize + SysvarAccountSize>(
    sysvar_data: &T,
    (lamports, rent_epoch): InheritableAccountFields,
) -> Account {
    let mut account = Account {
        lamports,
        data: vec![0; T::SIZE],
        owner: sysvar::id(),
        executable: false,
        rent_epoch,
    };
    to_account(sysvar_data, &mut account).unwrap();
    account
}

#[cfg(feature = "dev-context-only-utils")]
pub fn create_account_for_test<T: Serialize + SysvarAccountSize>(sysvar_data: &T) -> Account {
    create_account_with_fields(sysvar_data, (1, solana_clock::INITIAL_RENT_EPOCH))
}

pub fn create_account_shared_data_with_fields<T: Serialize + SysvarAccountSize>(
    sysvar_data: &T,
    fields: InheritableAccountFields,
) -> AccountSharedData {
    AccountSharedData::from(create_account_with_fields(sysvar_data, fields))
}

#[cfg(feature = "dev-context-only-utils")]
pub fn create_account_shared_data_for_test<T: Serialize + SysvarAccountSize>(
    sysvar_data: &T,
) -> AccountSharedData {
    AccountSharedData::from(create_account_for_test(sysvar_data))
}

pub fn from_account<T: DeserializeOwned + SysvarId, U: ReadableAccount>(account: &U) -> Option<T> {
    bincode::deserialize(account.data()).ok()
}

pub fn to_account<T: Serialize + SysvarAccountSize, U: WritableAccount>(
    sysvar_data: &T,
    account: &mut U,
) -> Option<()> {
    if T::SIZE > account.data().len() {
        return None;
    }
    bincode::serialize_into(account.data_as_mut_slice(), sysvar_data).ok()
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use {
        super::*,
        bincode::serialized_size,
        solana_hash::Hash,
        solana_slot_hashes::MAX_ENTRIES as SLOT_HASHES_MAX_ENTRIES,
        solana_stake_interface::stake_history::{
            MAX_ENTRIES as STAKE_HISTORY_MAX_ENTRIES, StakeHistoryEntry,
        },
        solana_sysvar::recent_blockhashes::{
            IterItem, MAX_ENTRIES as RECENT_BLOCKHASHES_MAX_ENTRIES,
        },
    };

    #[test]
    fn test_fixed_size_sysvar_account_sizes_match_bincode() {
        assert_eq!(
            Clock::SIZE,
            serialized_size(&Clock::default()).unwrap() as usize
        );
        assert_eq!(
            EpochRewards::SIZE,
            serialized_size(&EpochRewards::default()).unwrap() as usize
        );
        assert_eq!(
            EpochSchedule::SIZE,
            serialized_size(&EpochSchedule::default()).unwrap() as usize
        );
        assert_eq!(
            LastRestartSlot::SIZE,
            serialized_size(&LastRestartSlot::default()).unwrap() as usize
        );
        assert_eq!(
            Rent::SIZE,
            serialized_size(&Rent::default()).unwrap() as usize
        );
        assert_eq!(
            Rewards::SIZE,
            serialized_size(&Rewards::default()).unwrap() as usize
        );
        assert_eq!(
            Fees::SIZE,
            serialized_size(&Fees::default()).unwrap() as usize
        );
    }

    #[test]
    fn test_variable_size_sysvar_account_sizes_match_max_bincode() {
        let hash = Hash::default();
        let recent_blockhashes = (0..RECENT_BLOCKHASHES_MAX_ENTRIES as u64)
            .map(|block_height| IterItem(block_height, &hash, block_height))
            .collect::<RecentBlockhashes>();
        assert_eq!(
            RecentBlockhashes::SIZE,
            serialized_size(&recent_blockhashes).unwrap() as usize
        );

        let slot_hashes = (0..SLOT_HASHES_MAX_ENTRIES as u64)
            .map(|slot| (slot, hash))
            .collect::<SlotHashes>();
        assert_eq!(
            SlotHashes::SIZE,
            serialized_size(&slot_hashes).unwrap() as usize
        );

        let slot_history = SlotHistory::default();
        assert_eq!(
            SlotHistory::SIZE,
            serialized_size(&slot_history).unwrap() as usize
        );

        let mut stake_history = StakeHistory::default();
        for epoch in 0..STAKE_HISTORY_MAX_ENTRIES as u64 {
            stake_history.add(epoch, StakeHistoryEntry::default());
        }
        assert_eq!(
            StakeHistory::SIZE,
            serialized_size(&stake_history).unwrap() as usize
        );
    }
}
