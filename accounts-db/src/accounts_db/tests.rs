//! Accounts-db test suite.
#![cfg(test)]

use super::*;

mod append_vec;

pub use append_vec::DEFAULT_ACCOUNTS_DB_CONFIG as ACCOUNTS_DB_CONFIG_APPEND_VEC;

pub fn append_single_account_with_default_hash(
    storage: &AccountStorageEntry,
    pubkey: &Pubkey,
    account: &AccountSharedData,
    mark_alive: bool,
    add_to_index: Option<&AccountInfoAccountsIndex>,
) {
    let slot = storage.slot();
    let accounts = [(pubkey, account)];
    let slice = &accounts[..];
    let storable_accounts = (slot, slice);
    let stored_accounts_info = storage.accounts.write_accounts(&storable_accounts).unwrap();
    if mark_alive {
        // updates 'alive_bytes' on the storage
        storage.add_accounts(1, stored_accounts_info.size);
    }

    if let Some(index) = add_to_index {
        let account_info = AccountInfo::new(
            StorageLocation::AccountsFile(storage.id(), stored_accounts_info.offsets[0]),
            account.lamports() == 0,
        );
        index.upsert(
            slot,
            slot,
            pubkey,
            account_info,
            &mut ReclaimsSlotList::new(),
            UpsertReclaim::IgnoreReclaims,
        );
    }
}
