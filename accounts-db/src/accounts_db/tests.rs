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
        // Check the cache for the pubkey first; store_for_tests stores `slot` under `BankId::new(slot)`
        if let Some((cached, _slot)) = self.accounts_cache.load(BankId::new(slot), pubkey) {
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

/// Hashing a contiguous copy of the input must not change any account's lt hash.
#[test]
fn test_lt_hash_account_with_scratch_matches_streamed_input() {
    let pubkey = Pubkey::new_unique();
    let mut scratch = Vec::new();
    for data_len in [
        0,
        blake3::CHUNK_LEN - 1,
        blake3::CHUNK_LEN,
        3762,
        MAX_CONTIGUOUS_HASH_DATA_LEN,
        MAX_CONTIGUOUS_HASH_DATA_LEN + 1,
    ] {
        let account = AccountSharedData::from(Account {
            lamports: 123,
            data: (0..data_len).map(|i| i as u8).collect(),
            owner: Pubkey::new_unique(),
            executable: true,
            rent_epoch: 0,
        });
        assert_eq!(
            AccountsDb::lt_hash_account_with_scratch(&account, &pubkey, &mut scratch),
            AccountsDb::lt_hash_account(&account, &pubkey),
            "data_len={data_len}",
        );
    }
}
