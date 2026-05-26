#[allow(deprecated)]
use solana_sysvar::{fees::Fees, recent_blockhashes::RecentBlockhashes};
use {
    serde::{Serialize, de::DeserializeOwned},
    solana_account::{
        Account, AccountSharedData, InheritableAccountFields, ReadableAccount, WritableAccount,
    },
    solana_clock::Clock,
    solana_epoch_schedule::EpochSchedule,
    solana_program_runtime::sysvar_account::account_size,
    solana_rent::Rent,
    solana_sdk_ids::sysvar,
    solana_slot_hashes::SlotHashes,
    solana_slot_history::SlotHistory,
    solana_stake_interface::stake_history::StakeHistory,
    solana_sysvar::{
        epoch_rewards::EpochRewards, last_restart_slot::LastRestartSlot, rewards::Rewards,
    },
    solana_sysvar_id::SysvarId,
};

pub(crate) trait RuntimeSysvarAccount: SysvarId {
    const SIZE: usize;
}

/// One impl per sysvar Bank writes. Adding a new sysvar means one new row here.
macro_rules! impl_runtime_sysvar_account {
    ($($CONST:ident => $Ty:ty,)*) => {
        $(
            #[allow(deprecated)]
            impl RuntimeSysvarAccount for $Ty {
                const SIZE: usize = account_size::$CONST;
            }
        )*
    };
}

impl_runtime_sysvar_account! {
    CLOCK              => Clock,
    EPOCH_REWARDS      => EpochRewards,
    EPOCH_SCHEDULE     => EpochSchedule,
    LAST_RESTART_SLOT  => LastRestartSlot,
    RENT               => Rent,
    REWARDS            => Rewards,
    FEES               => Fees,
    RECENT_BLOCKHASHES => RecentBlockhashes,
    SLOT_HASHES        => SlotHashes,
    SLOT_HISTORY       => SlotHistory,
    STAKE_HISTORY      => StakeHistory,
}

pub(crate) fn create_account_shared_data_with_fields<T: Serialize + RuntimeSysvarAccount>(
    sysvar_data: &T,
    fields: InheritableAccountFields,
) -> AccountSharedData {
    create_account_shared_data_with_size_and_fields(sysvar_data, T::SIZE, fields)
}

pub(crate) fn create_account_shared_data_with_size_and_fields<T: Serialize>(
    sysvar_data: &T,
    size: usize,
    (lamports, rent_epoch): InheritableAccountFields,
) -> AccountSharedData {
    let mut account = Account {
        lamports,
        data: vec![0; size],
        owner: sysvar::id(),
        executable: false,
        rent_epoch,
    };
    to_account_with_size(sysvar_data, size, &mut account).unwrap();
    AccountSharedData::from(account)
}

pub(crate) fn from_account<T: DeserializeOwned, U: ReadableAccount>(account: &U) -> Option<T> {
    bincode::deserialize(account.data()).ok()
}

pub(crate) fn to_account<T: Serialize + RuntimeSysvarAccount, U: WritableAccount>(
    sysvar_data: &T,
    account: &mut U,
) -> Option<()> {
    to_account_with_size(sysvar_data, T::SIZE, account)
}

pub(crate) fn to_account_with_size<T: Serialize, U: WritableAccount>(
    sysvar_data: &T,
    size: usize,
    account: &mut U,
) -> Option<()> {
    if size > account.data().len() {
        return None;
    }
    bincode::serialize_into(account.data_as_mut_slice(), sysvar_data).ok()
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use {
        super::*,
        solana_hash::Hash,
        solana_slot_hashes::MAX_ENTRIES as SLOT_HASHES_MAX_ENTRIES,
        solana_stake_interface::stake_history::{
            MAX_ENTRIES as STAKE_HISTORY_MAX_ENTRIES, StakeHistoryEntry,
        },
        solana_sysvar::recent_blockhashes::{
            IterItem, MAX_ENTRIES as RECENT_BLOCKHASHES_MAX_ENTRIES,
        },
        test_case::test_case,
    };

    #[test_case(Clock::default(); "clock")]
    #[test_case(EpochRewards::default(); "epoch_rewards")]
    #[test_case(EpochSchedule::default(); "epoch_schedule")]
    #[test_case(LastRestartSlot::default(); "last_restart_slot")]
    #[test_case(Rent::default(); "rent")]
    #[test_case(Rewards::default(); "rewards")]
    #[test_case(Fees::default(); "fees")]
    fn test_runtime_sysvar_account_fixed_sizes_match_bincode<
        T: Default + Serialize + RuntimeSysvarAccount,
    >(
        _: T,
    ) {
        let actual = bincode::serialized_size(&T::default()).unwrap() as usize;
        assert_eq!(T::SIZE, actual);
    }

    #[test]
    fn test_runtime_sysvar_account_variable_sizes_match_max_bincode() {
        let hash = Hash::default();

        let recent_blockhashes = (0..RECENT_BLOCKHASHES_MAX_ENTRIES as u64)
            .map(|h| IterItem(h, &hash, h))
            .collect::<RecentBlockhashes>();
        assert_eq!(
            <RecentBlockhashes as RuntimeSysvarAccount>::SIZE,
            bincode::serialized_size(&recent_blockhashes).unwrap() as usize,
        );

        let slot_hashes = (0..SLOT_HASHES_MAX_ENTRIES as u64)
            .map(|s| (s, hash))
            .collect::<SlotHashes>();
        assert_eq!(
            <SlotHashes as RuntimeSysvarAccount>::SIZE,
            bincode::serialized_size(&slot_hashes).unwrap() as usize,
        );

        let slot_history = SlotHistory::default();
        assert_eq!(
            <SlotHistory as RuntimeSysvarAccount>::SIZE,
            bincode::serialized_size(&slot_history).unwrap() as usize,
        );

        let mut stake_history = StakeHistory::default();
        for epoch in 0..STAKE_HISTORY_MAX_ENTRIES as u64 {
            stake_history.add(epoch, StakeHistoryEntry::default());
        }
        assert_eq!(
            <StakeHistory as RuntimeSysvarAccount>::SIZE,
            bincode::serialized_size(&stake_history).unwrap() as usize,
        );
    }
}
