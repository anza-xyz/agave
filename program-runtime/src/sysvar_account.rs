use {
    serde::Serialize,
    solana_account::{Account, AccountSharedData},
    solana_clock::INITIAL_RENT_EPOCH,
    solana_pubkey::Pubkey,
    solana_sdk_ids::sysvar,
    solana_sysvar_id::SysvarId,
};

macro_rules! sysvar_registry {
    ($($CONST:ident = $size:expr => $Ty:ty,)*) => {
        pub mod account_size {
            $(pub const $CONST: usize = $size;)*
        }

        /// Returns the on-chain account size for the sysvar with the given
        /// pubkey, or `None` if the pubkey is not a registered sysvar.
        #[allow(deprecated)]
        pub fn size_for_id(sysvar_id: &Pubkey) -> Option<usize> {
            $(
                if <$Ty as SysvarId>::check_id(sysvar_id) {
                    return Some(account_size::$CONST);
                }
            )*
            None
        }
    };
}

// TODO: Temporary until sizes are within each sysvar crate
sysvar_registry! {
    CLOCK              = 40      => solana_clock::Clock,
    EPOCH_REWARDS      = 81      => solana_epoch_rewards::EpochRewards,
    EPOCH_SCHEDULE     = 33      => solana_epoch_schedule::EpochSchedule,
    LAST_RESTART_SLOT  = 8       => solana_last_restart_slot::LastRestartSlot,
    RENT               = 17      => solana_rent::Rent,
    REWARDS            = 16      => solana_sysvar::rewards::Rewards,
    FEES               = 8       => solana_sysvar::fees::Fees,
    RECENT_BLOCKHASHES = 6_008   => solana_sysvar::recent_blockhashes::RecentBlockhashes,
    SLOT_HASHES        = 20_488  => solana_slot_hashes::SlotHashes,
    SLOT_HISTORY       = 131_097 => solana_sysvar::slot_history::SlotHistory,
    STAKE_HISTORY      = 16_392  => solana_stake_interface::stake_history::StakeHistory,
}

pub fn account_size_of<T: SysvarId>() -> Option<usize> {
    size_for_id(&T::id())
}

pub fn create_account_shared_data_for_test<T: Serialize + SysvarId>(
    value: &T,
) -> AccountSharedData {
    let size = account_size_of::<T>().unwrap_or_else(|| panic!("unsupported sysvar: {}", T::id()));
    let mut account = Account {
        lamports: 1,
        data: vec![0; size],
        owner: sysvar::id(),
        executable: false,
        rent_epoch: INITIAL_RENT_EPOCH,
    };
    bincode::serialize_into(&mut account.data[..], value).unwrap();
    AccountSharedData::from(account)
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use {
        super::account_size,
        bincode::serialized_size,
        solana_clock::Clock,
        solana_epoch_rewards::EpochRewards,
        solana_epoch_schedule::EpochSchedule,
        solana_hash::Hash,
        solana_last_restart_slot::LastRestartSlot,
        solana_rent::Rent,
        solana_slot_hashes::{MAX_ENTRIES as SLOT_HASHES_MAX_ENTRIES, SlotHashes},
        solana_stake_interface::stake_history::{
            MAX_ENTRIES as STAKE_HISTORY_MAX_ENTRIES, StakeHistory, StakeHistoryEntry,
        },
        solana_sysvar::{
            fees::Fees,
            recent_blockhashes::{
                IterItem, MAX_ENTRIES as RECENT_BLOCKHASHES_MAX_ENTRIES, RecentBlockhashes,
            },
            rewards::Rewards,
            slot_history::SlotHistory,
        },
    };

    #[test]
    fn test_fixed_size_sysvar_account_sizes_match_bincode() {
        assert_eq!(
            account_size::CLOCK,
            serialized_size(&Clock::default()).unwrap() as usize
        );
        assert_eq!(
            account_size::EPOCH_REWARDS,
            serialized_size(&EpochRewards::default()).unwrap() as usize
        );
        assert_eq!(
            account_size::EPOCH_SCHEDULE,
            serialized_size(&EpochSchedule::default()).unwrap() as usize
        );
        assert_eq!(
            account_size::LAST_RESTART_SLOT,
            serialized_size(&LastRestartSlot::default()).unwrap() as usize
        );
        assert_eq!(
            account_size::RENT,
            serialized_size(&Rent::default()).unwrap() as usize
        );
        assert_eq!(
            account_size::REWARDS,
            serialized_size(&Rewards::default()).unwrap() as usize
        );
        assert_eq!(
            account_size::FEES,
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
            account_size::RECENT_BLOCKHASHES,
            serialized_size(&recent_blockhashes).unwrap() as usize
        );

        let slot_hashes = (0..SLOT_HASHES_MAX_ENTRIES as u64)
            .map(|slot| (slot, hash))
            .collect::<SlotHashes>();
        assert_eq!(
            account_size::SLOT_HASHES,
            serialized_size(&slot_hashes).unwrap() as usize
        );

        let slot_history = SlotHistory::default();
        assert_eq!(
            account_size::SLOT_HISTORY,
            serialized_size(&slot_history).unwrap() as usize
        );

        let mut stake_history = StakeHistory::default();
        for epoch in 0..STAKE_HISTORY_MAX_ENTRIES as u64 {
            stake_history.add(epoch, StakeHistoryEntry::default());
        }
        assert_eq!(
            account_size::STAKE_HISTORY,
            serialized_size(&stake_history).unwrap() as usize
        );
    }
}
