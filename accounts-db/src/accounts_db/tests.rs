//! Accounts-db test suite.
#![cfg(test)]

use {
    super::*,
    solana_account::{AccountSharedData, ReadableAccount},
};

mod append_vec;

// re-export these fns that live in impl.rs because ancient append vec tests use them...
pub(crate) use append_vec::r#impl::{
    append_single_account_with_default_hash, compare_all_accounts,
    get_account_from_account_from_storage, get_all_accounts, remove_account_for_tests,
};

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

                accessor.check_and_get_loaded_account_shared_data()
            },
        )
    }
}
