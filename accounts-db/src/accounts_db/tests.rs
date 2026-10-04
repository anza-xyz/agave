//! Accounts-db test suite.
#![cfg(test)]
#![allow(clippy::duplicate_mod)]

use super::*;

mod append_vec;
mod split_file;

pub use {
    append_vec::DEFAULT_ACCOUNTS_DB_CONFIG as ACCOUNTS_DB_CONFIG_APPEND_VEC,
    split_file::DEFAULT_ACCOUNTS_DB_CONFIG as ACCOUNTS_DB_CONFIG_SPLIT_FILE,
};

const NO_LOAD_FILTER: Option<fn(u64, &Pubkey, usize) -> bool> = None;

impl AccountsDb {
    fn get_storage_for_slot(&self, slot: Slot) -> Option<Arc<AccountStorageEntry>> {
        self.storage.get_slot_storage_entry(slot)
    }

    fn get_and_assert_single_storage(&self, slot: Slot) -> Arc<AccountStorageEntry> {
        self.storage.get_slot_storage_entry(slot).unwrap()
    }

    fn get_account_at_slot(&self, pubkey: &Pubkey, slot: Slot) -> Option<AccountSharedData> {
        // Check the cache for the pubkey first
        if let Some(cached) = self.accounts_cache.load(slot, pubkey) {
            return Some(cached.account.clone());
        }

        // Add the slot to ancestors so unrooted slots will be selected
        let mut ancestors = Ancestors::default();
        ancestors.insert(slot);

        self.accounts_index.get_with_and_then(
            pubkey,
            &ancestors,
            false,
            |(slot_found, account_info)| {
                // If a slot was found, ensure it was the requested slot
                assert_eq!(slot_found, slot);
                let storage_location = account_info.storage_location();
                let mut accessor = self.get_account_accessor(slot, &storage_location);

                accessor
                    .check_and_get_loaded_account_shared_data(NO_LOAD_FILTER)
                    .unwrap()
            },
        )
    }
}
