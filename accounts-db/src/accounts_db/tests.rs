//! Accounts-db test suite.
#![cfg(test)]

use {
    super::*,
    crate::storable_accounts::AccountForStorage,
    solana_account::{AccountSharedData, ReadableAccount},
};

mod append_vec;

// re-export these fns that live in impl.rs because ancient append vec tests use them...
pub(crate) use append_vec::r#impl::{
    append_single_account_with_default_hash, compare_all_accounts,
    create_db_with_storages_and_index, create_storages_and_update_index,
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

/// this tuple contains slot info PER account
impl<'a, T: ReadableAccount + Sync> StorableAccounts<'a> for (Slot, &'a [(&'a Pubkey, &'a T, Slot)])
where
    AccountForStorage<'a>: From<&'a T>,
{
    fn is_zero_lamport(&self, index: usize) -> bool {
        self.1[index].1.lamports() == 0
    }
    fn data_len(&self, index: usize) -> usize {
        self.1[index].1.data().len()
    }
    fn account<Ret>(
        &self,
        index: usize,
        mut callback: impl for<'local> FnMut(AccountForStorage<'local>) -> Ret,
    ) -> Ret {
        callback(self.1[index].1.into())
    }
    fn account_for_geyser<Ret>(
        &self,
        _index: usize,
        _callback: impl for<'local> FnMut(&'local Pubkey, &'local AccountSharedData) -> Ret,
    ) -> Ret {
        unimplemented!();
    }
    fn pubkey(&self, index: usize) -> &Pubkey {
        self.1[index].0
    }
    fn slot(&self, index: usize) -> Slot {
        // note that this could be different than 'target_slot()' PER account
        self.1[index].2
    }
    fn target_slot(&self) -> Slot {
        self.0
    }
    fn len(&self) -> usize {
        self.1.len()
    }
}
