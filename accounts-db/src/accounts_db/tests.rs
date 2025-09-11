#![cfg(test)]
use {
    super::*,
    crate::{
        accounts_file::AccountsFileProvider,
        accounts_index::{tests::*, AccountSecondaryIndexesIncludeExclude},
        ancient_append_vecs,
        append_vec::{
            aligned_stored_size, test_utils::TempFile, AccountMeta, AppendVec, StoredAccountMeta,
            StoredMeta,
        },
        storable_accounts::AccountForStorage,
    },
    itertools::Itertools,
    rand::{prelude::SliceRandom, thread_rng, Rng},
    solana_account::{
        accounts_equal, Account, AccountSharedData, InheritableAccountFields, ReadableAccount,
        WritableAccount, DUMMY_INHERITABLE_ACCOUNT_FIELDS,
    },
    solana_lattice_hash::lt_hash::Checksum as LtHashChecksum,
    solana_pubkey::PUBKEY_BYTES,
    std::{
        iter::{self, FromIterator},
        ops::Range,
        str::FromStr,
        sync::{atomic::AtomicBool, RwLock},
        thread::{self, Builder, JoinHandle},
    },
    test_case::test_case,
};

fn linear_ancestors(end_slot: u64) -> Ancestors {
    let mut ancestors: Ancestors = vec![(0, 0)].into_iter().collect();
    for i in 1..end_slot {
        ancestors.insert(i, (i - 1) as usize);
    }
    ancestors
}

impl AccountsDb {
    fn get_storage_for_slot(&self, slot: Slot) -> Option<Arc<AccountStorageEntry>> {
        self.storage.get_slot_storage_entry(slot)
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
    fn contains_multiple_slots(&self) -> bool {
        let len = self.len();
        if len > 0 {
            let slot = self.slot(0);
            // true if any item has a different slot than the first item
            (1..len).any(|i| slot != self.slot(i))
        } else {
            false
        }
    }
}

fn create_loadable_account_with_fields(
    name: &str,
    (lamports, rent_epoch): InheritableAccountFields,
) -> AccountSharedData {
    AccountSharedData::from(Account {
        lamports,
        owner: solana_sdk_ids::native_loader::id(),
        data: name.as_bytes().to_vec(),
        executable: true,
        rent_epoch,
    })
}

fn create_loadable_account_for_test(name: &str) -> AccountSharedData {
    create_loadable_account_with_fields(name, DUMMY_INHERITABLE_ACCOUNT_FIELDS)
}

impl AccountStorageEntry {
    fn add_account(&self, num_bytes: usize) {
        self.add_accounts(1, num_bytes)
    }
}

/// Helper macro to define accounts_db_test for both `AppendVec` and `HotStorage`.
/// This macro supports creating both regular tests and tests that should panic.
/// Usage:
///   For regular test, use the following syntax.
///     define_accounts_db_test!(TEST_NAME, |accounts_db| { TEST_BODY }); // regular test
///   For test that should panic, use the following syntax.
///     define_accounts_db_test!(TEST_NAME, panic = "PANIC_MSG", |accounts_db| { TEST_BODY });
macro_rules! define_accounts_db_test {
    (@testfn $name:ident, $accounts_file_provider: ident, |$accounts_db:ident| $inner: tt) => {
            fn run_test($accounts_db: AccountsDb) {
                $inner
            }
            let accounts_db =
                AccountsDb::new_single_for_tests_with_provider($accounts_file_provider);
            run_test(accounts_db);

    };
    ($name:ident, |$accounts_db:ident| $inner: tt) => {
        #[test_case(AccountsFileProvider::AppendVec; "append_vec")]
        #[test_case(AccountsFileProvider::HotStorage; "hot_storage")]
        fn $name(accounts_file_provider: AccountsFileProvider) {
            define_accounts_db_test!(@testfn $name, accounts_file_provider, |$accounts_db| $inner);
        }
    };
    ($name:ident, panic = $panic_message:literal, |$accounts_db:ident| $inner: tt) => {
        #[test_case(AccountsFileProvider::AppendVec; "append_vec")]
        #[test_case(AccountsFileProvider::HotStorage; "hot_storage")]
        #[should_panic(expected = $panic_message)]
        fn $name(accounts_file_provider: AccountsFileProvider) {
            define_accounts_db_test!(@testfn $name, accounts_file_provider, |$accounts_db| $inner);
        }
    };
}

pub(crate) use define_accounts_db_test;

fn run_generate_index_duplicates_within_slot_test(db: AccountsDb, reverse: bool) {
    let slot0 = 0;

    let pubkey = Pubkey::from([1; 32]);

    let append_vec = db.create_and_insert_store(slot0, 1000, "test");

    let mut account_small = AccountSharedData::default();
    account_small.set_data(vec![1]);
    account_small.set_lamports(1);
    let mut account_big = AccountSharedData::default();
    account_big.set_data(vec![5; 10]);
    account_big.set_lamports(2);
    assert_ne!(
        aligned_stored_size(account_big.data().len()),
        aligned_stored_size(account_small.data().len())
    );
    // same account twice with different data lens
    // Rules are the last one of each pubkey is the one that ends up in the index.
    let mut data = vec![(&pubkey, &account_big), (&pubkey, &account_small)];
    if reverse {
        data = data.into_iter().rev().collect();
    }
    let storable_accounts = (slot0, &data[..]);

    // construct append vec with account to generate an index from
    append_vec.accounts.write_accounts(&storable_accounts, 0);

    assert!(!db.accounts_index.contains(&pubkey));
    db.generate_index(None, false);
}

define_accounts_db_test!(
    test_generate_index_duplicates_within_slot,
    panic = "Accounts may only be stored once per slot:",
    |db| {
        run_generate_index_duplicates_within_slot_test(db, false);
    }
);

define_accounts_db_test!(
    test_generate_index_duplicates_within_slot_reverse,
    panic = "Accounts may only be stored once per slot:",
    |db| {
        run_generate_index_duplicates_within_slot_test(db, true);
    }
);

#[test]
fn test_generate_index_for_single_ref_zero_lamport_slot() {
    let db = AccountsDb::new_single_for_tests();
    let slot0 = 0;
    let pubkey = Pubkey::from([1; 32]);
    let append_vec = db.create_and_insert_store(slot0, 1000, "test");
    let account = AccountSharedData::default();

    let data = [(&pubkey, &account)];
    let storable_accounts = (slot0, &data[..]);
    append_vec.accounts.write_accounts(&storable_accounts, 0);
    assert!(!db.accounts_index.contains(&pubkey));
    let result = db.generate_index(None, false);
    let entry = db.accounts_index.get_cloned(&pubkey).unwrap();
    assert_eq!(entry.slot_list.read().unwrap().len(), 1);
    assert_eq!(append_vec.alive_bytes(), aligned_stored_size(0));
    assert_eq!(append_vec.accounts_count(), 1);
    assert_eq!(append_vec.count(), 1);
    assert_eq!(result.accounts_data_len, 0);
    assert_eq!(1, append_vec.num_zero_lamport_single_ref_accounts());
    assert_eq!(
        0,
        append_vec.alive_bytes_exclude_zero_lamport_single_ref_accounts()
    );
}

fn generate_sample_account_from_storage(i: u8) -> AccountFromStorage {
    // offset has to be 8 byte aligned
    let offset = (i as usize) * std::mem::size_of::<u64>();
    AccountFromStorage {
        index_info: AccountInfo::new(StorageLocation::AppendVec(i as u32, offset), i == 0),
        data_len: i as u64,
        pubkey: Pubkey::new_from_array([i; 32]),
    }
}

/// Reserve ancient storage size is not supported for TiredStorage
#[test]
fn test_sort_and_remove_dups() {
    // empty
    let mut test1 = vec![];
    let expected = test1.clone();
    AccountsDb::sort_and_remove_dups(&mut test1);
    assert_eq!(test1, expected);
    assert_eq!(test1, expected);
    // just 0
    let mut test1 = vec![generate_sample_account_from_storage(0)];
    let expected = test1.clone();
    AccountsDb::sort_and_remove_dups(&mut test1);
    assert_eq!(test1, expected);
    assert_eq!(test1, expected);
    // 0, 1
    let mut test1 = vec![
        generate_sample_account_from_storage(0),
        generate_sample_account_from_storage(1),
    ];
    let expected = test1.clone();
    AccountsDb::sort_and_remove_dups(&mut test1);
    assert_eq!(test1, expected);
    assert_eq!(test1, expected);
    // 1, 0. sort should reverse
    let mut test2 = vec![
        generate_sample_account_from_storage(1),
        generate_sample_account_from_storage(0),
    ];
    AccountsDb::sort_and_remove_dups(&mut test2);
    assert_eq!(test2, expected);
    assert_eq!(test2, expected);

    for insert_other_good in 0..2 {
        // 0 twice so it gets removed
        let mut test1 = vec![
            generate_sample_account_from_storage(0),
            generate_sample_account_from_storage(0),
        ];
        let mut expected = test1.clone();
        expected.truncate(1); // get rid of 1st duplicate
        test1.first_mut().unwrap().data_len = 2342342; // this one should be ignored, so modify the data_len so it will fail the compare below if it is used
        if insert_other_good < 2 {
            // insert another good one before or after the 2 bad ones
            test1.insert(insert_other_good, generate_sample_account_from_storage(1));
            // other good one should always be last since it is sorted after
            expected.push(generate_sample_account_from_storage(1));
        }
        AccountsDb::sort_and_remove_dups(&mut test1);
        assert_eq!(test1, expected);
        assert_eq!(test1, expected);
    }

    let mut test1 = [1, 0, 1, 0, 1u8]
        .into_iter()
        .map(generate_sample_account_from_storage)
        .collect::<Vec<_>>();
    test1.iter_mut().take(3).for_each(|entry| {
        entry.data_len = 2342342; // this one should be ignored, so modify the data_len so it will fail the compare below if it is used
        entry.index_info = AccountInfo::new(StorageLocation::Cached, false);
    });

    let expected = [0, 1u8]
        .into_iter()
        .map(generate_sample_account_from_storage)
        .collect::<Vec<_>>();
    AccountsDb::sort_and_remove_dups(&mut test1);
    assert_eq!(test1, expected);
    assert_eq!(test1, expected);
}

#[test]
fn test_sort_and_remove_dups_random() {
    use rand::prelude::*;
    let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(1234);
    let accounts: Vec<_> =
        std::iter::repeat_with(|| generate_sample_account_from_storage(rng.gen::<u8>()))
            .take(1000)
            .collect();

    let mut accounts1 = accounts.clone();
    let num_dups1 = AccountsDb::sort_and_remove_dups(&mut accounts1);

    // Use BTreeMap to calculate sort and remove dups alternatively.
    let mut map = std::collections::BTreeMap::default();
    let mut num_dups2 = 0;
    for account in accounts.iter() {
        if map.insert(*account.pubkey(), *account).is_some() {
            num_dups2 += 1;
        }
    }
    let accounts2: Vec<_> = map.into_values().collect();
    assert_eq!(accounts1, accounts2);
    assert_eq!(num_dups1, num_dups2);
}

pub(crate) fn append_single_account_with_default_hash(
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
    let stored_accounts_info = storage
        .accounts
        .write_accounts(&storable_accounts, 0)
        .unwrap();
    if mark_alive {
        // updates 'alive_bytes' on the storage
        storage.add_account(stored_accounts_info.size);
    }

    if let Some(index) = add_to_index {
        let account_info = AccountInfo::new(
            StorageLocation::AppendVec(storage.id(), stored_accounts_info.offsets[0]),
            account.lamports() == 0,
        );
        index.upsert(
            slot,
            slot,
            pubkey,
            account,
            &AccountSecondaryIndexes::default(),
            account_info,
            &mut Vec::default(),
            UpsertReclaim::IgnoreReclaims,
        );
    }
}

fn append_sample_data_to_storage(
    storage: &AccountStorageEntry,
    pubkey: &Pubkey,
    mark_alive: bool,
    account_data_size: Option<u64>,
) {
    let acc = AccountSharedData::new(
        1,
        account_data_size.unwrap_or(48) as usize,
        AccountSharedData::default().owner(),
    );
    append_single_account_with_default_hash(storage, pubkey, &acc, mark_alive, None);
}

fn sample_storage_with_entries_id_fill_percentage(
    tf: &TempFile,
    slot: Slot,
    pubkey: &Pubkey,
    id: AccountsFileId,
    mark_alive: bool,
    account_data_size: Option<u64>,
    fill_percentage: u64,
) -> Arc<AccountStorageEntry> {
    let (_temp_dirs, paths) = get_temp_accounts_paths(1).unwrap();
    let file_size = account_data_size.unwrap_or(123) * 100 / fill_percentage;
    let size_aligned: usize = aligned_stored_size(file_size as usize);
    let mut data = AccountStorageEntry::new(
        &paths[0],
        slot,
        id,
        size_aligned as u64,
        AccountsFileProvider::AppendVec,
    );
    let av = AccountsFile::AppendVec(AppendVec::new(
        &tf.path,
        true,
        (1024 * 1024).max(size_aligned),
    ));
    data.accounts = av;

    let arc = Arc::new(data);
    append_sample_data_to_storage(&arc, pubkey, mark_alive, account_data_size);
    arc
}

fn sample_storage_with_entries_id(
    tf: &TempFile,
    slot: Slot,
    pubkey: &Pubkey,
    id: AccountsFileId,
    mark_alive: bool,
    account_data_size: Option<u64>,
) -> Arc<AccountStorageEntry> {
    sample_storage_with_entries_id_fill_percentage(
        tf,
        slot,
        pubkey,
        id,
        mark_alive,
        account_data_size,
        100,
    )
}

define_accounts_db_test!(test_accountsdb_add_root, |db| {
    let key = Pubkey::default();
    let account0 = AccountSharedData::new(1, 0, &key);

    db.store_for_tests((0, [(&key, &account0)].as_slice()));
    db.add_root(0);
    let ancestors = vec![(1, 1)].into_iter().collect();
    assert_eq!(
        db.load_without_fixed_root(&ancestors, &key),
        Some((account0, 0))
    );
});

define_accounts_db_test!(test_accountsdb_latest_ancestor, |db| {
    let key = Pubkey::default();
    let account0 = AccountSharedData::new(1, 0, &key);

    db.store_for_tests((0, [(&key, &account0)].as_slice()));

    let account1 = AccountSharedData::new(0, 0, &key);
    db.store_for_tests((1, [(&key, &account1)].as_slice()));

    let ancestors = vec![(1, 1)].into_iter().collect();
    assert_eq!(
        &db.load_without_fixed_root(&ancestors, &key).unwrap().0,
        &account1
    );

    let ancestors = vec![(1, 1), (0, 0)].into_iter().collect();
    assert_eq!(
        &db.load_without_fixed_root(&ancestors, &key).unwrap().0,
        &account1
    );

    let mut accounts = Vec::new();
    db.unchecked_scan_accounts(
        "",
        &ancestors,
        |_, account, _| {
            accounts.push(account.take_account());
        },
        &ScanConfig::default(),
    );
    assert_eq!(accounts, vec![account1]);
});

define_accounts_db_test!(test_accountsdb_latest_ancestor_with_root, |db| {
    let key = Pubkey::default();
    let account0 = AccountSharedData::new(1, 0, &key);

    db.store_for_tests((0, [(&key, &account0)].as_slice()));

    let account1 = AccountSharedData::new(0, 0, &key);
    db.store_for_tests((1, [(&key, &account1)].as_slice()));
    db.add_root(0);

    let ancestors = vec![(1, 1)].into_iter().collect();
    assert_eq!(
        &db.load_without_fixed_root(&ancestors, &key).unwrap().0,
        &account1
    );

    let ancestors = vec![(1, 1), (0, 0)].into_iter().collect();
    assert_eq!(
        &db.load_without_fixed_root(&ancestors, &key).unwrap().0,
        &account1
    );
});

define_accounts_db_test!(test_accountsdb_root_one_slot, |db| {
    let key = Pubkey::default();
    let account0 = AccountSharedData::new(1, 0, &key);

    // store value 1 in the "root", i.e. db zero
    db.store_for_tests((0, [(&key, &account0)].as_slice()));

    // now we have:
    //
    //                       root0 -> key.lamports==1
    //                        / \
    //                       /   \
    //  key.lamports==0 <- slot1    \
    //                             slot2 -> key.lamports==1
    //                                       (via root0)

    // store value 0 in one child
    let account1 = AccountSharedData::new(0, 0, &key);
    db.store_for_tests((1, [(&key, &account1)].as_slice()));

    // masking accounts is done at the Accounts level, at accountsDB we see
    // original account (but could also accept "None", which is implemented
    // at the Accounts level)
    let ancestors = vec![(0, 0), (1, 1)].into_iter().collect();
    assert_eq!(
        &db.load_without_fixed_root(&ancestors, &key).unwrap().0,
        &account1
    );

    // we should see 1 token in slot 2
    let ancestors = vec![(0, 0), (2, 2)].into_iter().collect();
    assert_eq!(
        &db.load_without_fixed_root(&ancestors, &key).unwrap().0,
        &account0
    );

    db.add_root(0);

    let ancestors = vec![(1, 1)].into_iter().collect();
    assert_eq!(
        db.load_without_fixed_root(&ancestors, &key),
        Some((account1, 1))
    );
    let ancestors = vec![(2, 2)].into_iter().collect();
    assert_eq!(
        db.load_without_fixed_root(&ancestors, &key),
        Some((account0, 0))
    ); // original value
});

define_accounts_db_test!(test_accountsdb_add_root_many, |db| {
    let mut pubkeys: Vec<Pubkey> = vec![];
    db.create_account(&mut pubkeys, 0, 100, 0, 0);
    for _ in 1..100 {
        let idx = thread_rng().gen_range(0..99);
        let ancestors = vec![(0, 0)].into_iter().collect();
        let account = db
            .load_without_fixed_root(&ancestors, &pubkeys[idx])
            .unwrap();
        let default_account = AccountSharedData::from(Account {
            lamports: (idx + 1) as u64,
            ..Account::default()
        });
        assert_eq!((default_account, 0), account);
    }

    db.add_root(0);

    // check that all the accounts appear with a new root
    for _ in 1..100 {
        let idx = thread_rng().gen_range(0..99);
        let ancestors = vec![(0, 0)].into_iter().collect();
        let account0 = db
            .load_without_fixed_root(&ancestors, &pubkeys[idx])
            .unwrap();
        let ancestors = vec![(1, 1)].into_iter().collect();
        let account1 = db
            .load_without_fixed_root(&ancestors, &pubkeys[idx])
            .unwrap();
        let default_account = AccountSharedData::from(Account {
            lamports: (idx + 1) as u64,
            ..Account::default()
        });
        assert_eq!(&default_account, &account0.0);
        assert_eq!(&default_account, &account1.0);
    }
});

define_accounts_db_test!(test_accountsdb_count_stores, |db| {
    let mut pubkeys: Vec<Pubkey> = vec![];
    db.create_account(&mut pubkeys, 0, 2, DEFAULT_FILE_SIZE as usize / 3, 0);
    db.add_root_and_flush_write_cache(0);
    db.check_storage(0, 2, 2);

    let pubkey = solana_pubkey::new_rand();
    let account = AccountSharedData::new(1, DEFAULT_FILE_SIZE as usize / 3, &pubkey);
    db.store_for_tests((1, [(&pubkey, &account)].as_slice()));
    db.store_for_tests((1, [(&pubkeys[0], &account)].as_slice()));
    // adding root doesn't change anything
    db.add_root_and_flush_write_cache(1);
    {
        let slot_0_store = &db.storage.get_slot_storage_entry(0).unwrap();
        let slot_1_store = &db.storage.get_slot_storage_entry(1).unwrap();
        assert_eq!(slot_0_store.count(), 2);
        assert_eq!(slot_1_store.count(), 2);
        assert_eq!(slot_0_store.accounts_count(), 2);
        assert_eq!(slot_1_store.accounts_count(), 2);
    }

    // overwrite old rooted account version; only the r_slot_0_stores.count() should be
    // decremented
    // slot 2 is not a root and should be ignored by clean
    db.store_for_tests((2, [(&pubkeys[0], &account)].as_slice()));
    db.clean_accounts_for_tests();
    {
        let slot_0_store = &db.storage.get_slot_storage_entry(0).unwrap();
        let slot_1_store = &db.storage.get_slot_storage_entry(1).unwrap();
        assert_eq!(slot_0_store.count(), 1);
        assert_eq!(slot_1_store.count(), 2);
        assert_eq!(slot_0_store.accounts_count(), 2);
        assert_eq!(slot_1_store.accounts_count(), 2);
    }
});

define_accounts_db_test!(test_accounts_unsquashed, |db0| {
    let key = Pubkey::default();

    // 1 token in the "root", i.e. db zero
    let account0 = AccountSharedData::new(1, 0, &key);
    db0.store_for_tests((0, [(&key, &account0)].as_slice()));

    // 0 lamports in the child
    let account1 = AccountSharedData::new(0, 0, &key);
    db0.store_for_tests((1, [(&key, &account1)].as_slice()));

    // masking accounts is done at the Accounts level, at accountsDB we see
    // original account
    let ancestors = vec![(0, 0), (1, 1)].into_iter().collect();
    assert_eq!(
        db0.load_without_fixed_root(&ancestors, &key),
        Some((account1, 1))
    );
    let ancestors = vec![(0, 0)].into_iter().collect();
    assert_eq!(
        db0.load_without_fixed_root(&ancestors, &key),
        Some((account0, 0))
    );
});

/// Test to verify that reclaiming old storages during flush works correctly.
/// Creates multiple storages with accounts, flushes them, and then creates a new storage
/// that invalidates some of the old accounts. The test checks that one of the old storages
/// is reclaimed as the storage is fully invalidated
#[test]
fn test_flush_slots_with_reclaim_old_slots() {
    let accounts = AccountsDb::new_single_for_tests();
    let mut pubkeys = vec![];

    // Create and flush 5 slots with 5 accounts each
    for slot in 0..5 {
        let mut slot_pubkeys = vec![];
        for _ in 0..5 {
            let pubkey = solana_pubkey::new_rand();
            let account = AccountSharedData::new(slot + 1, 0, &pubkey);
            accounts.store_for_tests((slot, [(&pubkey, &account)].as_slice()));
            slot_pubkeys.push(pubkey);
        }
        pubkeys.push(slot_pubkeys);
        accounts.add_root_and_flush_write_cache(slot);
    }

    // Create another slot which invalidates 5 accounts from the first slot,
    // 4 accounts from the second slot, etc.
    let new_slot = 5;
    for (slot, slot_pubkeys) in pubkeys.iter().enumerate() {
        for pubkey in slot_pubkeys.iter().take(5 - slot) {
            let account = AccountSharedData::new(new_slot + 1, 0, pubkey);
            accounts.store_for_tests((new_slot, [(pubkey, &account)].as_slice()));
        }
    }

    // Get the accounts from the write cache slot
    let accounts_list: Vec<(_, _)> = accounts
        .accounts_cache
        .slot_cache(new_slot)
        .unwrap()
        .iter()
        .map(|iter_item| {
            let pubkey = *iter_item.key();
            let account = iter_item.value().account.clone();
            (pubkey, account)
        })
        .collect();

    let storage = accounts.create_and_insert_store(new_slot, 4096, "test_flush_slots");

    accounts.accounts_index.add_root(new_slot);

    // Flushing this storage directly using _store_accounts_frozen. This is done to pass in UpsertReclaim::ReclaimOldSlots
    accounts._store_accounts_frozen(
        (new_slot, &accounts_list[..]),
        &storage,
        UpsertReclaim::ReclaimOldSlots,
        UpdateIndexThreadSelection::Inline,
    );

    // Remove the flushed slot from the cache
    assert!(accounts.accounts_cache.remove_slot(new_slot).is_some());

    // Verify that the storage for the first slot has been removed
    assert!(accounts.storage.get_slot_storage_entry(0).is_none());
    for slot in 1..5 {
        assert!(accounts.storage.get_slot_storage_entry(slot).is_some());

        // Verify that the obsolete accounts for the remaining slots are correct
        let storage = accounts.storage.get_slot_storage_entry(slot).unwrap();
        assert_eq!(
            storage.get_obsolete_accounts(Some(new_slot)).len() as u64,
            5 - slot
        );
    }
    assert!(accounts.storage.get_slot_storage_entry(new_slot).is_some());
}

fn run_test_remove_unrooted_slot(is_cached: bool, db: AccountsDb) {
    let unrooted_slot = 9;
    let unrooted_bank_id = 9;
    let key = Pubkey::default();
    let account0 = AccountSharedData::new(1, 0, &key);
    let ancestors = vec![(unrooted_slot, 1)].into_iter().collect();
    assert!(!db.accounts_index.contains(&key));
    if is_cached {
        db.store_for_tests((unrooted_slot, &[(&key, &account0)][..]));
        assert!(db.accounts_cache.contains(unrooted_slot));
    } else {
        let file_size = 4096; // value doesn't need to be exact, just big enough to hold account0
        let storage = db.create_and_insert_store(unrooted_slot, file_size, "");
        db.store_accounts_frozen(
            (unrooted_slot, [(&key, &account0)].as_slice()),
            &storage,
            UpdateIndexThreadSelection::Inline,
        );
        assert!(db.storage.get_slot_storage_entry(unrooted_slot).is_some());
    }
    assert!(!db.accounts_index.is_alive_root(unrooted_slot));
    assert!(db.accounts_index.contains(&key));
    db.assert_load_account(unrooted_slot, key, 1);

    // Purge the slot
    db.remove_unrooted_slots(&[(unrooted_slot, unrooted_bank_id)]);
    assert!(db.load_without_fixed_root(&ancestors, &key).is_none());
    assert!(db.accounts_cache.slot_cache(unrooted_slot).is_none());
    assert!(db.storage.get_slot_storage_entry(unrooted_slot).is_none());
    assert!(!db.accounts_index.contains(&key));

    // Test we can store for the same slot again and get the right information
    let account0 = AccountSharedData::new(2, 0, &key);
    db.store_for_tests((unrooted_slot, [(&key, &account0)].as_slice()));
    db.assert_load_account(unrooted_slot, key, 2);
}

define_accounts_db_test!(test_remove_unrooted_slot_cached, |db| {
    run_test_remove_unrooted_slot(true, db);
});

define_accounts_db_test!(test_remove_unrooted_slot_storage, |db| {
    run_test_remove_unrooted_slot(false, db);
});

fn update_accounts(accounts: &AccountsDb, pubkeys: &[Pubkey], slot: Slot, range: usize) {
    for _ in 1..1000 {
        let idx = thread_rng().gen_range(0..range);
        let ancestors = vec![(slot, 0)].into_iter().collect();
        if let Some((mut account, _)) = accounts.load_without_fixed_root(&ancestors, &pubkeys[idx])
        {
            account.checked_add_lamports(1).unwrap();
            accounts.store_for_tests((slot, [(&pubkeys[idx], &account)].as_slice()));
            if account.is_zero_lamport() {
                let ancestors = vec![(slot, 0)].into_iter().collect();
                assert!(accounts
                    .load_without_fixed_root(&ancestors, &pubkeys[idx])
                    .is_none());
            } else {
                let default_account = AccountSharedData::from(Account {
                    lamports: account.lamports(),
                    ..Account::default()
                });
                assert_eq!(default_account, account);
            }
        }
    }
}

#[test]
fn test_account_one() {
    let (_accounts_dirs, paths) = get_temp_accounts_paths(1).unwrap();
    let db = AccountsDb::new_for_tests(paths);
    let mut pubkeys: Vec<Pubkey> = vec![];
    db.create_account(&mut pubkeys, 0, 1, 0, 0);
    let ancestors = vec![(0, 0)].into_iter().collect();
    let account = db.load_without_fixed_root(&ancestors, &pubkeys[0]).unwrap();
    let default_account = AccountSharedData::from(Account {
        lamports: 1,
        ..Account::default()
    });
    assert_eq!((default_account, 0), account);
}

#[test]
fn test_account_many() {
    let (_accounts_dirs, paths) = get_temp_accounts_paths(2).unwrap();
    let db = AccountsDb::new_for_tests(paths);
    let mut pubkeys: Vec<Pubkey> = vec![];
    db.create_account(&mut pubkeys, 0, 100, 0, 0);
    db.check_accounts(&pubkeys, 0, 100, 1);
}

#[test]
fn test_account_update() {
    let accounts = AccountsDb::new_single_for_tests();
    let mut pubkeys: Vec<Pubkey> = vec![];
    accounts.create_account(&mut pubkeys, 0, 100, 0, 0);
    update_accounts(&accounts, &pubkeys, 0, 99);
    accounts.add_root_and_flush_write_cache(0);
    accounts.check_storage(0, 100, 100);
}

#[test]
fn test_account_grow_many() {
    let (_accounts_dir, paths) = get_temp_accounts_paths(2).unwrap();
    let size = 4096;
    let accounts = AccountsDb {
        file_size: size,
        ..AccountsDb::new_for_tests(paths)
    };
    let mut keys = vec![];
    for i in 0..9 {
        let key = solana_pubkey::new_rand();
        let account = AccountSharedData::new(i + 1, size as usize / 4, &key);
        accounts.store_for_tests((0, [(&key, &account)].as_slice()));
        keys.push(key);
    }
    let ancestors = vec![(0, 0)].into_iter().collect();
    for (i, key) in keys.iter().enumerate() {
        assert_eq!(
            accounts
                .load_without_fixed_root(&ancestors, key)
                .unwrap()
                .0
                .lamports(),
            (i as u64) + 1
        );
    }

    let mut append_vec_histogram = HashMap::new();
    let mut all_slots = vec![];
    for slot_storage in accounts.storage.iter() {
        all_slots.push(slot_storage.0)
    }
    for slot in all_slots {
        *append_vec_histogram.entry(slot).or_insert(0) += 1;
    }
    for count in append_vec_histogram.values() {
        assert!(*count >= 2);
    }
}

#[test]
fn test_account_grow() {
    for pass in 0..27 {
        let accounts = AccountsDb::new_single_for_tests();

        let status = [AccountStorageStatus::Available, AccountStorageStatus::Full];
        let pubkey1 = solana_pubkey::new_rand();
        let account1 = AccountSharedData::new(1, DEFAULT_FILE_SIZE as usize / 2, &pubkey1);
        accounts.store_for_tests((0, [(&pubkey1, &account1)].as_slice()));
        if pass == 0 {
            accounts.add_root_and_flush_write_cache(0);
            let store = &accounts.storage.get_slot_storage_entry(0).unwrap();
            assert_eq!(store.count(), 1);
            assert_eq!(store.status(), AccountStorageStatus::Available);
            continue;
        }

        let pubkey2 = solana_pubkey::new_rand();
        let account2 = AccountSharedData::new(1, DEFAULT_FILE_SIZE as usize / 2, &pubkey2);
        accounts.store_for_tests((0, [(&pubkey2, &account2)].as_slice()));

        if pass == 1 {
            accounts.add_root_and_flush_write_cache(0);
            assert_eq!(accounts.storage.len(), 1);
            let store = &accounts.storage.get_slot_storage_entry(0).unwrap();
            assert_eq!(store.count(), 2);
            assert_eq!(store.status(), AccountStorageStatus::Available);
            continue;
        }
        let ancestors = vec![(0, 0)].into_iter().collect();
        assert_eq!(
            accounts
                .load_without_fixed_root(&ancestors, &pubkey1)
                .unwrap()
                .0,
            account1
        );
        assert_eq!(
            accounts
                .load_without_fixed_root(&ancestors, &pubkey2)
                .unwrap()
                .0,
            account2
        );

        // lots of writes, but they are all duplicates
        for i in 0..25 {
            accounts.store_for_tests((0, [(&pubkey1, &account1)].as_slice()));
            let flush = pass == i + 2;
            if flush {
                accounts.add_root_and_flush_write_cache(0);
                assert_eq!(accounts.storage.len(), 1);
                let store = &accounts.storage.get_slot_storage_entry(0).unwrap();
                assert_eq!(store.status(), status[0]);
            }
            let ancestors = vec![(0, 0)].into_iter().collect();
            assert_eq!(
                accounts
                    .load_without_fixed_root(&ancestors, &pubkey1)
                    .unwrap()
                    .0,
                account1
            );
            assert_eq!(
                accounts
                    .load_without_fixed_root(&ancestors, &pubkey2)
                    .unwrap()
                    .0,
                account2
            );
            if flush {
                break;
            }
        }
    }
}

#[test]
fn test_lazy_gc_slot() {
    solana_logger::setup();
    //This test is pedantic
    //A slot is purged when a non root bank is cleaned up.  If a slot is behind root but it is
    //not root, it means we are retaining dead banks.
    let accounts = AccountsDb::new_single_for_tests();
    let pubkey = solana_pubkey::new_rand();
    let account = AccountSharedData::new(1, 0, AccountSharedData::default().owner());
    //store an account
    accounts.store_for_tests((0, [(&pubkey, &account)].as_slice()));
    accounts.add_root_and_flush_write_cache(0);

    let ancestors = vec![(0, 0)].into_iter().collect();
    let id = accounts
        .accounts_index
        .get_with_and_then(
            &pubkey,
            Some(&ancestors),
            None,
            false,
            |(_slot, account_info)| account_info.store_id(),
        )
        .unwrap();

    //slot is still there, since gc is lazy
    assert_eq!(accounts.storage.get_slot_storage_entry(0).unwrap().id(), id);

    //store causes clean
    accounts.store_for_tests((1, [(&pubkey, &account)].as_slice()));

    //slot is gone
    accounts.print_accounts_stats("pre-clean");
    accounts.add_root_and_flush_write_cache(1);
    assert!(accounts.storage.get_slot_storage_entry(0).is_some());
    accounts.clean_accounts_for_tests();
    assert!(accounts.storage.get_slot_storage_entry(0).is_none());

    //new value is there
    let ancestors = vec![(1, 1)].into_iter().collect();
    assert_eq!(
        accounts.load_without_fixed_root(&ancestors, &pubkey),
        Some((account, 1))
    );
}

#[test]
fn test_clean_zero_lamport_and_dead_slot() {
    solana_logger::setup();

    let accounts = AccountsDb::new_single_for_tests();
    let pubkey1 = solana_pubkey::new_rand();
    let pubkey2 = solana_pubkey::new_rand();
    let account = AccountSharedData::new(1, 1, AccountSharedData::default().owner());
    let zero_lamport_account = AccountSharedData::new(0, 0, AccountSharedData::default().owner());

    // Store two accounts
    accounts.store_for_tests((0, [(&pubkey1, &account)].as_slice()));
    accounts.store_for_tests((0, [(&pubkey2, &account)].as_slice()));

    // Make sure both accounts are in the same AppendVec in slot 0, which
    // will prevent pubkey1 from being cleaned up later even when it's a
    // zero-lamport account
    let ancestors = vec![(0, 1)].into_iter().collect();
    let (slot1, account_info1) = accounts
        .accounts_index
        .get_with_and_then(
            &pubkey1,
            Some(&ancestors),
            None,
            false,
            |(slot, account_info)| (slot, account_info),
        )
        .unwrap();
    let (slot2, account_info2) = accounts
        .accounts_index
        .get_with_and_then(
            &pubkey2,
            Some(&ancestors),
            None,
            false,
            |(slot, account_info)| (slot, account_info),
        )
        .unwrap();
    assert_eq!(slot1, 0);
    assert_eq!(slot1, slot2);
    assert_eq!(account_info1.storage_location(), StorageLocation::Cached);
    assert_eq!(
        account_info1.storage_location(),
        account_info2.storage_location()
    );

    // Update account 1 in slot 1
    accounts.store_for_tests((1, [(&pubkey1, &account)].as_slice()));

    // Update account 1 as  zero lamports account
    accounts.store_for_tests((2, [(&pubkey1, &zero_lamport_account)].as_slice()));

    // Pubkey 1 was the only account in slot 1, and it was updated in slot 2, so
    // slot 1 should be purged
    accounts.add_root_and_flush_write_cache(0);
    accounts.add_root_and_flush_write_cache(1);
    accounts.add_root_and_flush_write_cache(2);

    // Slot 1 should be removed, slot 0 cannot be removed because it still has
    // the latest update for pubkey 2
    accounts.clean_accounts_for_tests();
    assert!(accounts.storage.get_slot_storage_entry(0).is_some());
    assert!(accounts.storage.get_slot_storage_entry(1).is_none());

    // Slot 1 should be cleaned because all it's accounts are
    // zero lamports, and are not present in any other slot's
    // storage entries
    assert_eq!(accounts.alive_account_count_in_slot(1), 0);
}

#[test]
fn test_clean_dead_slot_with_obsolete_accounts() {
    solana_logger::setup();

    // This test is triggering a scenario in reclaim_accounts where the entire slot is reclaimed
    // When an entire slot is reclaimed, it normally unrefs the pubkeys, while when individual
    // accounts are reclaimed it does not unref the pubkeys

    // Obsolete accounts are already unreffed so they should not be unreffed again

    let accounts = AccountsDb::new_single_for_tests();
    let pubkey = solana_pubkey::new_rand();
    let account = AccountSharedData::new(1, 1, AccountSharedData::default().owner());

    // Store account 1 in slot 0
    accounts.store_for_tests((0, [(&pubkey, &account)].as_slice()));

    // Update account 1 as in slot 1
    accounts.store_for_tests((1, [(&pubkey, &account)].as_slice()));

    // Update account 1 as in slot 2
    accounts.store_for_tests((2, [(&pubkey, &account)].as_slice()));

    // Flush the slots individually to avoid reclaims
    accounts.add_root_and_flush_write_cache(0);
    accounts.add_root_and_flush_write_cache(1);
    accounts.add_root_and_flush_write_cache(2);

    // Pubkey1 should be in 3 slots, 0 and 1 and 2
    assert_eq!(accounts.accounts_index.ref_count_from_storage(&pubkey), 3);

    // Mark pubkey in slot 1 as obsolete, simulating obsolete accounts being enabled
    let old_storage = accounts
        .storage
        .get_slot_storage_entry_shrinking_in_progress_ok(1)
        .unwrap();
    old_storage.mark_accounts_obsolete(vec![(0, 1)].into_iter(), 2);

    // Unreference pubkey, which would occur during the normal mark_accounts_obsolete flow
    accounts.unref_pubkeys([pubkey].iter(), 1, &HashSet::new());

    // Pubkey1 should now have two references: Slot0 and Slot2.
    assert_eq!(accounts.accounts_index.ref_count_from_storage(&pubkey), 2);

    // Clean, remove slot0/1.
    accounts.clean_accounts_for_tests();
    assert!(accounts.storage.get_slot_storage_entry(0).is_none());
    assert!(accounts.storage.get_slot_storage_entry(1).is_none());

    // Ref count for pubkey should be 1. It was decremented for slot1 and above, and decremented
    // for slot0 during clean_accounts_for_tests
    // It was NOT decremented for slot1 during clean_accounts_for_test as it was marked obsolete
    assert_eq!(accounts.accounts_index.ref_count_from_storage(&pubkey), 1);
}

#[test]
#[should_panic(expected = "ref count expected to be zero")]
fn test_remove_zero_lamport_multi_ref_accounts_panic() {
    let accounts = AccountsDb::new_single_for_tests();
    let pubkey_zero = Pubkey::from([1; 32]);
    let one_lamport_account = AccountSharedData::new(1, 0, AccountSharedData::default().owner());

    let zero_lamport_account = AccountSharedData::new(0, 0, AccountSharedData::default().owner());
    let slot = 1;

    accounts.store_for_tests((slot, [(&pubkey_zero, &one_lamport_account)].as_slice()));
    accounts.add_root_and_flush_write_cache(slot);

    accounts.store_for_tests((slot + 1, [(&pubkey_zero, &zero_lamport_account)].as_slice()));
    accounts.add_root_and_flush_write_cache(slot + 1);

    // This should panic because there are 2 refs for pubkey_zero.
    accounts.remove_zero_lamport_single_ref_accounts_after_shrink(
        &[&pubkey_zero],
        slot,
        &ShrinkStats::default(),
        true,
    );
}

#[test]
fn test_remove_zero_lamport_single_ref_accounts_after_shrink() {
    for pass in 0..3 {
        let accounts = AccountsDb::new_single_for_tests();
        let pubkey_zero = Pubkey::from([1; 32]);
        let pubkey2 = Pubkey::from([2; 32]);
        let account = AccountSharedData::new(1, 0, AccountSharedData::default().owner());
        let zero_lamport_account =
            AccountSharedData::new(0, 0, AccountSharedData::default().owner());
        let slot = 1;

        accounts.store_for_tests((
            slot,
            [(&pubkey_zero, &zero_lamport_account), (&pubkey2, &account)].as_slice(),
        ));

        // Simulate rooting the zero-lamport account, writes it to storage
        accounts.add_root_and_flush_write_cache(slot);

        if pass > 0 {
            // store in write cache
            accounts
                .store_for_tests((slot + 1, [(&pubkey_zero, &zero_lamport_account)].as_slice()));
            if pass == 2 {
                // move to a storage (causing ref count to increase)
                accounts.add_root_and_flush_write_cache(slot + 1);
            }
        }

        accounts.accounts_index.get_and_then(&pubkey_zero, |entry| {
            let expected_ref_count = if pass < 2 { 1 } else { 2 };
            assert_eq!(entry.unwrap().ref_count(), expected_ref_count, "{pass}");
            let expected_slot_list = if pass < 1 { 1 } else { 2 };
            assert_eq!(
                entry.unwrap().slot_list.read().unwrap().len(),
                expected_slot_list
            );
            (false, ())
        });
        accounts.accounts_index.get_and_then(&pubkey2, |entry| {
            assert!(entry.is_some());
            (false, ())
        });

        let zero_lamport_single_ref_pubkeys = if pass < 2 { vec![&pubkey_zero] } else { vec![] };
        accounts.remove_zero_lamport_single_ref_accounts_after_shrink(
            &zero_lamport_single_ref_pubkeys,
            slot,
            &ShrinkStats::default(),
            true,
        );

        accounts.accounts_index.get_and_then(&pubkey_zero, |entry| {
            match pass {
                0 => {
                    // should not exist in index at all
                    assert!(entry.is_none(), "{pass}");
                }
                1 => {
                    // alive only in slot + 1
                    assert_eq!(entry.unwrap().slot_list.read().unwrap().len(), 1);
                    assert_eq!(
                        entry
                            .unwrap()
                            .slot_list
                            .read()
                            .unwrap()
                            .first()
                            .map(|(s, _)| s)
                            .cloned()
                            .unwrap(),
                        slot + 1
                    );
                    let expected_ref_count = 0;
                    assert_eq!(
                        entry.map(|e| e.ref_count()),
                        Some(expected_ref_count),
                        "{pass}"
                    );
                }
                2 => {
                    // alive in both slot, slot + 1
                    assert_eq!(entry.unwrap().slot_list.read().unwrap().len(), 2);

                    let slots = entry
                        .unwrap()
                        .slot_list
                        .read()
                        .unwrap()
                        .iter()
                        .map(|(s, _)| s)
                        .cloned()
                        .collect::<Vec<_>>();
                    assert_eq!(slots, vec![slot, slot + 1]);
                    let expected_ref_count = 2;
                    assert_eq!(
                        entry.map(|e| e.ref_count()),
                        Some(expected_ref_count),
                        "{pass}"
                    );
                }
                _ => {
                    unreachable!("Shouldn't reach here.")
                }
            }
            (false, ())
        });

        accounts.accounts_index.get_and_then(&pubkey2, |entry| {
            assert!(entry.is_some(), "{pass}");
            (false, ())
        });
    }
}

#[test]
fn test_shrink_zero_lamport_single_ref_account() {
    solana_logger::setup();
    // note that 'None' checks the case based on the default value of `latest_full_snapshot_slot` in `AccountsDb`
    for latest_full_snapshot_slot in [None, Some(0), Some(1), Some(2)] {
        // store a zero and non-zero lamport account
        // make sure clean marks the ref_count=1, zero lamport account dead and removes pubkey from index completely
        let accounts = AccountsDb::new_single_for_tests();
        let pubkey_zero = Pubkey::from([1; 32]);
        let pubkey2 = Pubkey::from([2; 32]);
        let account = AccountSharedData::new(1, 0, AccountSharedData::default().owner());
        let zero_lamport_account =
            AccountSharedData::new(0, 0, AccountSharedData::default().owner());
        let slot = 1;
        // Store a zero-lamport account and a non-zero lamport account
        accounts.store_for_tests((
            slot,
            [(&pubkey_zero, &zero_lamport_account), (&pubkey2, &account)].as_slice(),
        ));

        // Simulate rooting the zero-lamport account, should be a
        // candidate for cleaning
        accounts.add_root_and_flush_write_cache(slot);

        // for testing, we need to cause shrink to think this will be productive.
        // The zero lamport account isn't dead, but it can become dead inside shrink.
        accounts
            .storage
            .get_slot_storage_entry(slot)
            .unwrap()
            .alive_bytes
            .fetch_sub(aligned_stored_size(0), Ordering::Release);

        if let Some(latest_full_snapshot_slot) = latest_full_snapshot_slot {
            accounts.set_latest_full_snapshot_slot(latest_full_snapshot_slot);
        }

        // Shrink the slot. The behavior on the zero lamport account will depend on `latest_full_snapshot_slot`.
        accounts.shrink_slot_forced(slot);

        assert!(
            accounts.storage.get_slot_storage_entry(1).is_some(),
            "{latest_full_snapshot_slot:?}"
        );

        let expected_alive_count = if latest_full_snapshot_slot.unwrap_or(Slot::MAX) < slot {
            // zero lamport account should NOT be dead in the index
            assert!(
                accounts
                    .accounts_index
                    .contains_with(&pubkey_zero, None, None),
                "{latest_full_snapshot_slot:?}"
            );
            2
        } else {
            // zero lamport account should be dead in the index
            assert!(
                !accounts
                    .accounts_index
                    .contains_with(&pubkey_zero, None, None),
                "{latest_full_snapshot_slot:?}"
            );
            // the zero lamport account should be marked as dead
            1
        };

        assert_eq!(
            accounts.alive_account_count_in_slot(slot),
            expected_alive_count,
            "{latest_full_snapshot_slot:?}"
        );

        // other account should still be alive
        assert!(
            accounts.accounts_index.contains_with(&pubkey2, None, None),
            "{latest_full_snapshot_slot:?}"
        );
        assert!(
            accounts.storage.get_slot_storage_entry(slot).is_some(),
            "{latest_full_snapshot_slot:?}"
        );
    }
}

#[test]
fn test_clean_multiple_zero_lamport_decrements_index_ref_count() {
    solana_logger::setup();

    let accounts = AccountsDb::new_single_for_tests();
    let pubkey1 = solana_pubkey::new_rand();
    let pubkey2 = solana_pubkey::new_rand();
    let zero_lamport_account = AccountSharedData::new(0, 0, AccountSharedData::default().owner());

    // Store 2 accounts in slot 0, then update account 1 in two more slots
    accounts.store_for_tests((0, [(&pubkey1, &zero_lamport_account)].as_slice()));
    accounts.store_for_tests((0, [(&pubkey2, &zero_lamport_account)].as_slice()));
    accounts.store_for_tests((1, [(&pubkey1, &zero_lamport_account)].as_slice()));
    accounts.store_for_tests((2, [(&pubkey1, &zero_lamport_account)].as_slice()));
    // Root all slots
    accounts.add_root_and_flush_write_cache(0);
    accounts.add_root_and_flush_write_cache(1);
    accounts.add_root_and_flush_write_cache(2);

    // Account ref counts should match how many slots they were stored in
    // Account 1 = 3 slots; account 2 = 1 slot
    assert_eq!(accounts.accounts_index.ref_count_from_storage(&pubkey1), 3);
    assert_eq!(accounts.accounts_index.ref_count_from_storage(&pubkey2), 1);

    accounts.clean_accounts_for_tests();
    // Slots 0 and 1 should each have been cleaned because all of their
    // accounts are zero lamports
    assert!(accounts.storage.get_slot_storage_entry(0).is_none());
    assert!(accounts.storage.get_slot_storage_entry(1).is_none());
    // Slot 2 only has a zero lamport account as well. But, calc_delete_dependencies()
    // should exclude slot 2 from the clean due to changes in other slots
    assert!(accounts.storage.get_slot_storage_entry(2).is_some());
    // Index ref counts should be consistent with the slot stores. Account 1 ref count
    // should be 1 since slot 2 is the only alive slot; account 2 should have a ref
    // count of 0 due to slot 0 being dead
    assert_eq!(accounts.accounts_index.ref_count_from_storage(&pubkey1), 1);
    assert_eq!(accounts.accounts_index.ref_count_from_storage(&pubkey2), 0);

    accounts.clean_accounts_for_tests();
    // Slot 2 will now be cleaned, which will leave account 1 with a ref count of 0
    assert!(accounts.storage.get_slot_storage_entry(2).is_none());
    assert_eq!(accounts.accounts_index.ref_count_from_storage(&pubkey1), 0);
}

#[test]
fn test_clean_zero_lamport_and_old_roots() {
    solana_logger::setup();

    let accounts = AccountsDb::new_single_for_tests();
    let pubkey = solana_pubkey::new_rand();
    let account = AccountSharedData::new(1, 0, AccountSharedData::default().owner());
    let zero_lamport_account = AccountSharedData::new(0, 0, AccountSharedData::default().owner());

    // Store a zero-lamport account
    accounts.store_for_tests((0, [(&pubkey, &account)].as_slice()));
    accounts.store_for_tests((1, [(&pubkey, &zero_lamport_account)].as_slice()));

    // Simulate rooting the zero-lamport account, should be a
    // candidate for cleaning
    accounts.add_root_and_flush_write_cache(0);
    accounts.add_root_and_flush_write_cache(1);

    // Slot 0 should be removed, and
    // zero-lamport account should be cleaned
    accounts.clean_accounts_for_tests();

    assert!(accounts.storage.get_slot_storage_entry(0).is_none());
    assert!(accounts.storage.get_slot_storage_entry(1).is_none());

    // Slot 0 should be cleaned because all it's accounts have been
    // updated in the rooted slot 1
    assert_eq!(accounts.alive_account_count_in_slot(0), 0);

    // Slot 1 should be cleaned because all it's accounts are
    // zero lamports, and are not present in any other slot's
    // storage entries
    assert_eq!(accounts.alive_account_count_in_slot(1), 0);

    // zero lamport account, should no longer exist in accounts index
    // because it has been removed
    assert!(!accounts.accounts_index.contains_with(&pubkey, None, None));
}

#[test]
fn test_clean_old_with_normal_account() {
    solana_logger::setup();

    let accounts = AccountsDb::new_single_for_tests();
    let pubkey = solana_pubkey::new_rand();
    let account = AccountSharedData::new(1, 0, AccountSharedData::default().owner());
    //store an account
    accounts.store_for_tests((0, [(&pubkey, &account)].as_slice()));
    accounts.store_for_tests((1, [(&pubkey, &account)].as_slice()));

    // simulate slots are rooted after while
    accounts.add_root_and_flush_write_cache(0);
    accounts.add_root_and_flush_write_cache(1);

    //even if rooted, old state isn't cleaned up
    assert_eq!(accounts.alive_account_count_in_slot(0), 1);
    assert_eq!(accounts.alive_account_count_in_slot(1), 1);

    accounts.clean_accounts_for_tests();

    //now old state is cleaned up
    assert_eq!(accounts.alive_account_count_in_slot(0), 0);
    assert_eq!(accounts.alive_account_count_in_slot(1), 1);
}

#[test]
fn test_clean_old_with_zero_lamport_account() {
    solana_logger::setup();

    let accounts = AccountsDb::new_single_for_tests();
    let pubkey1 = solana_pubkey::new_rand();
    let pubkey2 = solana_pubkey::new_rand();
    let normal_account = AccountSharedData::new(1, 0, AccountSharedData::default().owner());
    let zero_account = AccountSharedData::new(0, 0, AccountSharedData::default().owner());
    //store an account
    accounts.store_for_tests((0, [(&pubkey1, &normal_account)].as_slice()));
    accounts.store_for_tests((1, [(&pubkey1, &zero_account)].as_slice()));
    accounts.store_for_tests((0, [(&pubkey2, &normal_account)].as_slice()));
    accounts.store_for_tests((1, [(&pubkey2, &normal_account)].as_slice()));

    //simulate slots are rooted after while
    accounts.add_root_and_flush_write_cache(0);
    accounts.add_root_and_flush_write_cache(1);

    //even if rooted, old state isn't cleaned up
    assert_eq!(accounts.alive_account_count_in_slot(0), 2);
    assert_eq!(accounts.alive_account_count_in_slot(1), 2);

    accounts.print_accounts_stats("");

    accounts.clean_accounts_for_tests();

    //Old state behind zero-lamport account is cleaned up
    assert_eq!(accounts.alive_account_count_in_slot(0), 0);
    assert_eq!(accounts.alive_account_count_in_slot(1), 2);
}

#[test]
fn test_clean_old_with_both_normal_and_zero_lamport_accounts() {
    solana_logger::setup();

    let mut accounts = AccountsDb {
        account_indexes: spl_token_mint_index_enabled(),
        ..AccountsDb::new_single_for_tests()
    };
    let pubkey1 = solana_pubkey::new_rand();
    let pubkey2 = solana_pubkey::new_rand();

    // Set up account to be added to secondary index
    const SPL_TOKEN_INITIALIZED_OFFSET: usize = 108;
    let mint_key = Pubkey::new_unique();
    let mut account_data_with_mint = vec![0; spl_generic_token::token::Account::get_packed_len()];
    account_data_with_mint[..PUBKEY_BYTES].clone_from_slice(&(mint_key.to_bytes()));
    account_data_with_mint[SPL_TOKEN_INITIALIZED_OFFSET] = 1;

    let mut normal_account = AccountSharedData::new(1, 0, AccountSharedData::default().owner());
    normal_account.set_owner(spl_generic_token::token::id());
    normal_account.set_data(account_data_with_mint.clone());
    let mut zero_account = AccountSharedData::new(0, 0, AccountSharedData::default().owner());
    zero_account.set_owner(spl_generic_token::token::id());
    zero_account.set_data(account_data_with_mint);

    //store an account
    accounts.store_for_tests((0, [(&pubkey1, &normal_account)].as_slice()));
    accounts.store_for_tests((0, [(&pubkey1, &normal_account)].as_slice()));
    accounts.store_for_tests((1, [(&pubkey1, &zero_account)].as_slice()));
    accounts.store_for_tests((0, [(&pubkey2, &normal_account)].as_slice()));
    accounts.store_for_tests((2, [(&pubkey2, &normal_account)].as_slice()));

    //simulate slots are rooted after while
    accounts.add_root_and_flush_write_cache(0);
    accounts.add_root_and_flush_write_cache(1);
    accounts.add_root_and_flush_write_cache(2);

    //even if rooted, old state isn't cleaned up
    assert_eq!(accounts.alive_account_count_in_slot(0), 2);
    assert_eq!(accounts.alive_account_count_in_slot(1), 1);
    assert_eq!(accounts.alive_account_count_in_slot(2), 1);

    // Secondary index should still find both pubkeys
    let mut found_accounts = HashSet::new();
    let index_key = IndexKey::SplTokenMint(mint_key);
    let bank_id = 0;
    accounts
        .accounts_index
        .index_scan_accounts(
            &Ancestors::default(),
            bank_id,
            index_key,
            |key, _| {
                found_accounts.insert(*key);
            },
            &ScanConfig::default(),
        )
        .unwrap();
    assert_eq!(found_accounts.len(), 2);
    assert!(found_accounts.contains(&pubkey1));
    assert!(found_accounts.contains(&pubkey2));

    {
        accounts.account_indexes.keys = Some(AccountSecondaryIndexesIncludeExclude {
            exclude: true,
            keys: [mint_key].iter().cloned().collect::<HashSet<Pubkey>>(),
        });
        // Secondary index can't be used - do normal scan: should still find both pubkeys
        let mut found_accounts = HashSet::new();
        let used_index = accounts
            .index_scan_accounts(
                &Ancestors::default(),
                bank_id,
                index_key,
                |account| {
                    found_accounts.insert(*account.unwrap().0);
                },
                &ScanConfig::default(),
            )
            .unwrap();
        assert!(!used_index);
        assert_eq!(found_accounts.len(), 2);
        assert!(found_accounts.contains(&pubkey1));
        assert!(found_accounts.contains(&pubkey2));

        accounts.account_indexes.keys = None;

        // Secondary index can now be used since it isn't marked as excluded
        let mut found_accounts = HashSet::new();
        let used_index = accounts
            .index_scan_accounts(
                &Ancestors::default(),
                bank_id,
                index_key,
                |account| {
                    found_accounts.insert(*account.unwrap().0);
                },
                &ScanConfig::default(),
            )
            .unwrap();
        assert!(used_index);
        assert_eq!(found_accounts.len(), 2);
        assert!(found_accounts.contains(&pubkey1));
        assert!(found_accounts.contains(&pubkey2));

        accounts.account_indexes.keys = None;
    }

    accounts.clean_accounts_for_tests();

    //both zero lamport and normal accounts are cleaned up
    assert_eq!(accounts.alive_account_count_in_slot(0), 0);
    // The only store to slot 1 was a zero lamport account, should
    // be purged by zero-lamport cleaning logic because slot 1 is
    // rooted
    assert_eq!(accounts.alive_account_count_in_slot(1), 0);
    assert_eq!(accounts.alive_account_count_in_slot(2), 1);

    // `pubkey1`, a zero lamport account, should no longer exist in accounts index
    // because it has been removed by the clean
    assert!(!accounts.accounts_index.contains_with(&pubkey1, None, None));

    // Secondary index should have purged `pubkey1` as well
    let mut found_accounts = vec![];
    accounts
        .accounts_index
        .index_scan_accounts(
            &Ancestors::default(),
            bank_id,
            IndexKey::SplTokenMint(mint_key),
            |key, _| found_accounts.push(*key),
            &ScanConfig::default(),
        )
        .unwrap();
    assert_eq!(found_accounts, vec![pubkey2]);
}

#[test]
fn test_clean_max_slot_zero_lamport_account() {
    solana_logger::setup();

    let accounts = AccountsDb::new_single_for_tests();
    let pubkey = solana_pubkey::new_rand();
    let account = AccountSharedData::new(1, 0, AccountSharedData::default().owner());
    let zero_account = AccountSharedData::new(0, 0, AccountSharedData::default().owner());

    // store an account, make it a zero lamport account
    // in slot 1
    accounts.store_for_tests((0, [(&pubkey, &account)].as_slice()));
    accounts.store_for_tests((1, [(&pubkey, &zero_account)].as_slice()));

    // simulate slots are rooted after while
    accounts.add_root_and_flush_write_cache(0);
    accounts.add_root_and_flush_write_cache(1);

    // Only clean up to account 0, should not purge slot 0 based on
    // updates in later slots in slot 1
    assert_eq!(accounts.alive_account_count_in_slot(0), 1);
    assert_eq!(accounts.alive_account_count_in_slot(1), 1);
    accounts.clean_accounts(Some(0), false, &EpochSchedule::default());
    assert_eq!(accounts.alive_account_count_in_slot(0), 1);
    assert_eq!(accounts.alive_account_count_in_slot(1), 1);
    assert!(accounts.accounts_index.contains_with(&pubkey, None, None));

    // Now the account can be cleaned up
    accounts.clean_accounts(Some(1), false, &EpochSchedule::default());
    assert_eq!(accounts.alive_account_count_in_slot(0), 0);
    assert_eq!(accounts.alive_account_count_in_slot(1), 0);

    // The zero lamport account, should no longer exist in accounts index
    // because it has been removed
    assert!(!accounts.accounts_index.contains_with(&pubkey, None, None));
}

fn assert_no_stores(accounts: &AccountsDb, slot: Slot) {
    let store = accounts.storage.get_slot_storage_entry(slot);
    assert!(store.is_none());
}

#[test]
fn test_accounts_db_purge_keep_live() {
    solana_logger::setup();
    let some_lamport = 223;
    let zero_lamport = 0;
    let no_data = 0;
    let owner = *AccountSharedData::default().owner();

    let account = AccountSharedData::new(some_lamport, no_data, &owner);
    let pubkey = solana_pubkey::new_rand();

    let account2 = AccountSharedData::new(some_lamport, no_data, &owner);
    let pubkey2 = solana_pubkey::new_rand();

    let zero_lamport_account = AccountSharedData::new(zero_lamport, no_data, &owner);

    let accounts = AccountsDb::new_single_for_tests();
    accounts.add_root_and_flush_write_cache(0);

    // Step A
    let mut current_slot = 1;
    accounts.store_for_tests((current_slot, [(&pubkey, &account)].as_slice()));
    // Store another live account to slot 1 which will prevent any purge
    // since the store count will not be zero
    accounts.store_for_tests((current_slot, [(&pubkey2, &account2)].as_slice()));
    accounts.add_root_and_flush_write_cache(current_slot);
    let (slot1, account_info1) = accounts
        .accounts_index
        .get_with_and_then(&pubkey, None, None, false, |(slot, account_info)| {
            (slot, account_info)
        })
        .unwrap();
    let (slot2, account_info2) = accounts
        .accounts_index
        .get_with_and_then(&pubkey2, None, None, false, |(slot, account_info)| {
            (slot, account_info)
        })
        .unwrap();
    assert_eq!(slot1, current_slot);
    assert_eq!(slot1, slot2);
    assert_eq!(account_info1.store_id(), account_info2.store_id());

    // Step B
    current_slot += 1;
    let zero_lamport_slot = current_slot;
    accounts.store_for_tests((current_slot, [(&pubkey, &zero_lamport_account)].as_slice()));
    accounts.add_root_and_flush_write_cache(current_slot);

    accounts.assert_load_account(current_slot, pubkey, zero_lamport);

    current_slot += 1;
    accounts.add_root_and_flush_write_cache(current_slot);

    accounts.print_accounts_stats("pre_purge");

    accounts.clean_accounts_for_tests();

    accounts.print_accounts_stats("post_purge");

    // The earlier entry for pubkey in the account index is purged,
    let (slot_list_len, index_slot) = {
        let account_entry = accounts.accounts_index.get_cloned(&pubkey).unwrap();
        let slot_list = account_entry.slot_list.read().unwrap();
        (slot_list.len(), slot_list[0].0)
    };
    assert_eq!(slot_list_len, 1);
    // Zero lamport entry was not the one purged
    assert_eq!(index_slot, zero_lamport_slot);
    // The ref count should still be 2 because no slots were purged
    assert_eq!(accounts.ref_count_for_pubkey(&pubkey), 2);

    // storage for slot 1 had 2 accounts, now has 1 after pubkey 1
    // was reclaimed
    accounts.check_storage(1, 1, 2);
    // storage for slot 2 had 1 accounts, now has 1
    accounts.check_storage(2, 1, 1);
}

#[test]
fn test_accounts_db_purge1() {
    solana_logger::setup();
    let some_lamport = 223;
    let zero_lamport = 0;
    let no_data = 0;
    let owner = *AccountSharedData::default().owner();

    let account = AccountSharedData::new(some_lamport, no_data, &owner);
    let pubkey = solana_pubkey::new_rand();

    let zero_lamport_account = AccountSharedData::new(zero_lamport, no_data, &owner);

    let accounts = AccountsDb::new_single_for_tests();
    accounts.add_root(0);

    let mut current_slot = 1;
    accounts.store_for_tests((current_slot, [(&pubkey, &account)].as_slice()));
    accounts.add_root_and_flush_write_cache(current_slot);

    current_slot += 1;
    accounts.store_for_tests((current_slot, [(&pubkey, &zero_lamport_account)].as_slice()));
    accounts.add_root_and_flush_write_cache(current_slot);

    accounts.assert_load_account(current_slot, pubkey, zero_lamport);

    // Otherwise slot 2 will not be removed
    current_slot += 1;
    accounts.add_root_and_flush_write_cache(current_slot);

    accounts.print_accounts_stats("pre_purge");

    let ancestors = linear_ancestors(current_slot);
    info!("ancestors: {ancestors:?}");
    let hash = accounts.calculate_accounts_lt_hash_at_startup_from_index(&ancestors, current_slot);

    accounts.clean_accounts_for_tests();

    assert_eq!(
        accounts.calculate_accounts_lt_hash_at_startup_from_index(&ancestors, current_slot),
        hash
    );

    accounts.print_accounts_stats("post_purge");

    // Make sure the index is for pubkey cleared
    assert!(!accounts.accounts_index.contains(&pubkey));

    // slot 1 & 2 should not have any stores
    assert_no_stores(&accounts, 1);
    assert_no_stores(&accounts, 2);
}

#[test]
#[ignore]
fn test_store_account_stress() {
    let slot = 42;
    let num_threads = 2;

    let min_file_bytes = std::mem::size_of::<StoredMeta>() + std::mem::size_of::<AccountMeta>();

    let db = Arc::new(AccountsDb {
        file_size: min_file_bytes as u64,
        ..AccountsDb::new_single_for_tests()
    });

    db.add_root(slot);
    let thread_hdls: Vec<_> = (0..num_threads)
        .map(|_| {
            let db = db.clone();
            std::thread::Builder::new()
                .name("account-writers".to_string())
                .spawn(move || {
                    let pubkey = solana_pubkey::new_rand();
                    let mut account = AccountSharedData::new(1, 0, &pubkey);
                    let mut i = 0;
                    loop {
                        let account_bal = thread_rng().gen_range(1..99);
                        account.set_lamports(account_bal);
                        db.store_for_tests((slot, [(&pubkey, &account)].as_slice()));

                        let (account, slot) = db
                            .load_without_fixed_root(&Ancestors::default(), &pubkey)
                            .unwrap_or_else(|| {
                                panic!("Could not fetch stored account {pubkey}, iter {i}")
                            });
                        assert_eq!(slot, slot);
                        assert_eq!(account.lamports(), account_bal);
                        i += 1;
                    }
                })
                .unwrap()
        })
        .collect();

    for t in thread_hdls {
        t.join().unwrap();
    }
}

#[test]
fn test_accountsdb_scan_accounts() {
    solana_logger::setup();
    let db = AccountsDb::new_single_for_tests();
    let key = Pubkey::default();
    let key0 = solana_pubkey::new_rand();
    let account0 = AccountSharedData::new(1, 0, &key);

    db.store_for_tests((0, [(&key0, &account0)].as_slice()));

    let key1 = solana_pubkey::new_rand();
    let account1 = AccountSharedData::new(2, 0, &key);
    db.store_for_tests((1, [(&key1, &account1)].as_slice()));

    let ancestors = vec![(0, 0)].into_iter().collect();
    let mut accounts = Vec::new();
    db.unchecked_scan_accounts(
        "",
        &ancestors,
        |_, account, _| {
            accounts.push(account.take_account());
        },
        &ScanConfig::default(),
    );
    assert_eq!(accounts, vec![account0]);

    let ancestors = vec![(1, 1), (0, 0)].into_iter().collect();
    let mut accounts = Vec::new();
    db.unchecked_scan_accounts(
        "",
        &ancestors,
        |_, account, _| {
            accounts.push(account.take_account());
        },
        &ScanConfig::default(),
    );
    assert_eq!(accounts.len(), 2);
}

#[test]
fn test_cleanup_key_not_removed() {
    solana_logger::setup();
    let db = AccountsDb::new_single_for_tests();

    let key = Pubkey::default();
    let key0 = solana_pubkey::new_rand();
    let account0 = AccountSharedData::new(1, 0, &key);

    db.store_for_tests((0, [(&key0, &account0)].as_slice()));

    let key1 = solana_pubkey::new_rand();
    let account1 = AccountSharedData::new(2, 0, &key);
    db.store_for_tests((1, [(&key1, &account1)].as_slice()));

    db.print_accounts_stats("pre");

    let slots: HashSet<Slot> = vec![1].into_iter().collect();
    let purge_keys = [(key1, slots)];
    let _ = db.purge_keys_exact(purge_keys.iter());

    let account2 = AccountSharedData::new(3, 0, &key);
    db.store_for_tests((2, [(&key1, &account2)].as_slice()));

    db.print_accounts_stats("post");
    let ancestors = vec![(2, 0)].into_iter().collect();
    assert_eq!(
        db.load_without_fixed_root(&ancestors, &key1)
            .unwrap()
            .0
            .lamports(),
        3
    );
}

#[test]
fn test_store_large_account() {
    solana_logger::setup();
    let db = AccountsDb::new_single_for_tests();

    let key = Pubkey::default();
    let data_len = DEFAULT_FILE_SIZE as usize + 7;
    let account = AccountSharedData::new(1, data_len, &key);

    db.store_for_tests((0, [(&key, &account)].as_slice()));

    let ancestors = vec![(0, 0)].into_iter().collect();
    let ret = db.load_without_fixed_root(&ancestors, &key).unwrap();
    assert_eq!(ret.0.data().len(), data_len);
}

#[test]
fn test_stored_readable_account() {
    let lamports = 1;
    let owner = Pubkey::new_unique();
    let executable = true;
    let rent_epoch = 2;
    let meta = StoredMeta {
        write_version_obsolete: 5,
        pubkey: Pubkey::new_unique(),
        data_len: 7,
    };
    let account_meta = AccountMeta {
        lamports,
        owner,
        executable,
        rent_epoch,
    };
    let data = Vec::new();
    let account = Account {
        lamports,
        owner,
        executable,
        rent_epoch,
        data: data.clone(),
    };
    let offset = 99 * std::mem::size_of::<u64>(); // offset needs to be 8 byte aligned
    let stored_size = 101;
    let stored_account = StoredAccountMeta {
        meta: &meta,
        account_meta: &account_meta,
        data: &data,
        offset,
        stored_size,
    };
    assert!(accounts_equal(&account, &stored_account));
}

#[test]
fn test_hash_stored_account() {
    // Number are just sequential.
    let meta = StoredMeta {
        write_version_obsolete: 0x09_0a_0b_0c_0d_0e_0f_10,
        data_len: 0x11_12_13_14_15_16_17_18,
        pubkey: Pubkey::from([
            0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26,
            0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f, 0x30, 0x31, 0x32, 0x33, 0x34,
            0x35, 0x36, 0x37, 0x38,
        ]),
    };
    let account_meta = AccountMeta {
        lamports: 0x39_3a_3b_3c_3d_3e_3f_40,
        rent_epoch: 0x41_42_43_44_45_46_47_48,
        owner: Pubkey::from([
            0x49, 0x4a, 0x4b, 0x4c, 0x4d, 0x4e, 0x4f, 0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56,
            0x57, 0x58, 0x59, 0x5a, 0x5b, 0x5c, 0x5d, 0x5e, 0x5f, 0x60, 0x61, 0x62, 0x63, 0x64,
            0x65, 0x66, 0x67, 0x68,
        ]),
        executable: false,
    };
    const ACCOUNT_DATA_LEN: usize = 3;
    let data: [u8; ACCOUNT_DATA_LEN] = [0x69, 0x6a, 0x6b];
    let offset: usize = 0x6c_6d_6e_6f_70_71_72_73;

    let stored_account = StoredAccountMeta {
        meta: &meta,
        account_meta: &account_meta,
        data: &data,
        offset,
        stored_size: 0,
    };
    let account = stored_account.to_account_shared_data();

    let expected_account_hash = LtHashChecksum([
        160, 29, 105, 138, 56, 166, 40, 55, 224, 231, 29, 208, 68, 46, 190, 89, 141, 20, 65, 86,
        115, 14, 182, 125, 174, 181, 165, 0, 72, 175, 105, 177,
    ]);
    assert_eq!(
        AccountsDb::lt_hash_account(&stored_account, stored_account.pubkey())
            .0
            .checksum(),
        expected_account_hash,
        "StoredAccountMeta's data layout might be changed; update hashing if needed."
    );
    assert_eq!(
        AccountsDb::lt_hash_account(&account, stored_account.pubkey())
            .0
            .checksum(),
        expected_account_hash,
        "Account-based hashing must be consistent with StoredAccountMeta-based one."
    );
}

// something we can get a ref to

pub static EPOCH_SCHEDULE: std::sync::LazyLock<EpochSchedule> =
    std::sync::LazyLock::new(EpochSchedule::default);

#[test]
fn test_verify_bank_capitalization() {
    for pass in 0..2 {
        solana_logger::setup();
        let db = AccountsDb::new_single_for_tests();

        let key = solana_pubkey::new_rand();
        let some_data_len = 0;
        let some_slot: Slot = 0;
        let account = AccountSharedData::new(1, some_data_len, &key);
        let ancestors = vec![(some_slot, 0)].into_iter().collect();

        db.store_for_tests((some_slot, [(&key, &account)].as_slice()));
        if pass == 0 {
            db.add_root_and_flush_write_cache(some_slot);

            assert_eq!(
                db.calculate_capitalization_at_startup_from_index(&ancestors, some_slot),
                1
            );
            continue;
        }

        let native_account_pubkey = solana_pubkey::new_rand();
        db.store_for_tests((
            some_slot,
            [(
                &native_account_pubkey,
                &create_loadable_account_for_test("foo"),
            )]
            .as_slice(),
        ));
        db.add_root_and_flush_write_cache(some_slot);

        assert_eq!(
            db.calculate_capitalization_at_startup_from_index(&ancestors, some_slot),
            2
        );
    }
}
#[test]
fn test_storage_finder() {
    solana_logger::setup();
    let db = AccountsDb {
        file_size: 16 * 1024,
        ..AccountsDb::new_single_for_tests()
    };
    let key = solana_pubkey::new_rand();
    let lamports = 100;
    let data_len = 8190;
    let account = AccountSharedData::new(lamports, data_len, &solana_pubkey::new_rand());
    // pre-populate with a smaller empty store
    db.create_and_insert_store(1, 8192, "test_storage_finder");
    db.store_for_tests((1, [(&key, &account)].as_slice()));
}

#[test]
fn test_get_snapshot_storages_empty() {
    let db = AccountsDb::new_single_for_tests();
    assert!(db.get_storages(..=0).0.is_empty());
}

#[test]
fn test_get_snapshot_storages_only_older_than_or_equal_to_snapshot_slot() {
    let db = AccountsDb::new_single_for_tests();

    let key = Pubkey::default();
    let account = AccountSharedData::new(1, 0, &key);
    let before_slot = 0;
    let base_slot = before_slot + 1;
    let after_slot = base_slot + 1;

    db.store_for_tests((base_slot, [(&key, &account)].as_slice()));
    db.add_root_and_flush_write_cache(base_slot);
    assert!(db.get_storages(..=before_slot).0.is_empty());

    assert_eq!(1, db.get_storages(..=base_slot).0.len());
    assert_eq!(1, db.get_storages(..=after_slot).0.len());
}

#[test]
fn test_get_snapshot_storages_only_non_empty() {
    for pass in 0..2 {
        let db = AccountsDb::new_single_for_tests();

        let key = Pubkey::default();
        let account = AccountSharedData::new(1, 0, &key);
        let base_slot = 0;
        let after_slot = base_slot + 1;

        db.store_for_tests((base_slot, [(&key, &account)].as_slice()));
        if pass == 0 {
            db.add_root_and_flush_write_cache(base_slot);
            db.storage.remove(&base_slot, false);
            assert!(db.get_storages(..=after_slot).0.is_empty());
            continue;
        }

        db.store_for_tests((base_slot, [(&key, &account)].as_slice()));
        db.add_root_and_flush_write_cache(base_slot);
        assert_eq!(1, db.get_storages(..=after_slot).0.len());
    }
}

#[test]
fn test_get_snapshot_storages_only_roots() {
    let db = AccountsDb::new_single_for_tests();

    let key = Pubkey::default();
    let account = AccountSharedData::new(1, 0, &key);
    let base_slot = 0;
    let after_slot = base_slot + 1;

    db.store_for_tests((base_slot, [(&key, &account)].as_slice()));
    assert!(db.get_storages(..=after_slot).0.is_empty());

    db.add_root_and_flush_write_cache(base_slot);
    assert_eq!(1, db.get_storages(..=after_slot).0.len());
}

#[test]
fn test_get_snapshot_storages_exclude_empty() {
    let db = AccountsDb::new_single_for_tests();

    let key = Pubkey::default();
    let account = AccountSharedData::new(1, 0, &key);
    let base_slot = 0;
    let after_slot = base_slot + 1;

    db.store_for_tests((base_slot, [(&key, &account)].as_slice()));
    db.add_root_and_flush_write_cache(base_slot);
    assert_eq!(1, db.get_storages(..=after_slot).0.len());

    db.storage
        .get_slot_storage_entry(0)
        .unwrap()
        .remove_accounts(0, 1);
    assert!(db.get_storages(..=after_slot).0.is_empty());
}

#[test]
fn test_get_snapshot_storages_with_base_slot() {
    let db = AccountsDb::new_single_for_tests();

    let key = Pubkey::default();
    let account = AccountSharedData::new(1, 0, &key);

    let slot = 10;
    db.store_for_tests((slot, [(&key, &account)].as_slice()));
    db.add_root_and_flush_write_cache(slot);
    assert_eq!(0, db.get_storages(slot + 1..=slot + 1).0.len());
    assert_eq!(1, db.get_storages(slot..=slot + 1).0.len());
}

define_accounts_db_test!(
    test_storage_remove_account_double_remove,
    panic = "double remove of account in slot: 0/store: 0!!",
    |accounts| {
        let pubkey = solana_pubkey::new_rand();
        let account = AccountSharedData::new(1, 0, AccountSharedData::default().owner());
        accounts.store_for_tests((0, [(&pubkey, &account)].as_slice()));
        accounts.add_root_and_flush_write_cache(0);
        let storage_entry = accounts.storage.get_slot_storage_entry(0).unwrap();
        storage_entry.remove_accounts(0, 1);
        storage_entry.remove_accounts(0, 1);
    }
);

fn do_full_clean_refcount(mut accounts: AccountsDb, store1_first: bool, store_size: u64) {
    let pubkey1 = Pubkey::from_str("My11111111111111111111111111111111111111111").unwrap();
    let pubkey2 = Pubkey::from_str("My22211111111111111111111111111111111111111").unwrap();
    let pubkey3 = Pubkey::from_str("My33311111111111111111111111111111111111111").unwrap();

    let old_lamport = 223;
    let zero_lamport = 0;
    let dummy_lamport = 999_999;

    // size data so only 1 fits in a 4k store
    let data_size = 2200;

    let owner = *AccountSharedData::default().owner();

    let account = AccountSharedData::new(old_lamport, data_size, &owner);
    let account2 = AccountSharedData::new(old_lamport + 100_001, data_size, &owner);
    let account3 = AccountSharedData::new(old_lamport + 100_002, data_size, &owner);
    let account4 = AccountSharedData::new(dummy_lamport, data_size, &owner);
    let zero_lamport_account = AccountSharedData::new(zero_lamport, data_size, &owner);

    let mut current_slot = 0;
    accounts.file_size = store_size;

    // A: Initialize AccountsDb with pubkey1 and pubkey2
    current_slot += 1;
    if store1_first {
        accounts.store_for_tests((current_slot, [(&pubkey1, &account)].as_slice()));
        accounts.store_for_tests((current_slot, [(&pubkey2, &account)].as_slice()));
    } else {
        accounts.store_for_tests((current_slot, [(&pubkey2, &account)].as_slice()));
        accounts.store_for_tests((current_slot, [(&pubkey1, &account)].as_slice()));
    }
    accounts.add_root_and_flush_write_cache(current_slot);

    info!("post A");
    accounts.print_accounts_stats("Post-A");

    // B: Test multiple updates to pubkey1 in a single slot/storage
    current_slot += 1;
    assert_eq!(0, accounts.alive_account_count_in_slot(current_slot));
    assert_eq!(1, accounts.ref_count_for_pubkey(&pubkey1));
    accounts.store_for_tests((current_slot, [(&pubkey1, &account2)].as_slice()));
    accounts.store_for_tests((current_slot, [(&pubkey1, &account2)].as_slice()));
    accounts.add_root_and_flush_write_cache(current_slot);
    assert_eq!(1, accounts.alive_account_count_in_slot(current_slot));
    // Stores to same pubkey, same slot only count once towards the
    // ref count
    assert_eq!(2, accounts.ref_count_for_pubkey(&pubkey1));
    accounts.add_root_and_flush_write_cache(current_slot);

    accounts.print_accounts_stats("Post-B pre-clean");

    accounts.clean_accounts_for_tests();

    info!("post B");
    accounts.print_accounts_stats("Post-B");

    // C: more updates to trigger clean of previous updates
    current_slot += 1;
    assert_eq!(2, accounts.ref_count_for_pubkey(&pubkey1));
    accounts.store_for_tests((current_slot, [(&pubkey1, &account3)].as_slice()));
    accounts.store_for_tests((current_slot, [(&pubkey2, &account3)].as_slice()));
    accounts.store_for_tests((current_slot, [(&pubkey3, &account4)].as_slice()));
    accounts.add_root_and_flush_write_cache(current_slot);
    assert_eq!(3, accounts.ref_count_for_pubkey(&pubkey1));

    info!("post C");

    accounts.print_accounts_stats("Post-C");

    // D: Make all keys 0-lamport, cleans all keys
    current_slot += 1;
    assert_eq!(3, accounts.ref_count_for_pubkey(&pubkey1));
    accounts.store_for_tests((current_slot, [(&pubkey1, &zero_lamport_account)].as_slice()));
    accounts.store_for_tests((current_slot, [(&pubkey2, &zero_lamport_account)].as_slice()));
    accounts.store_for_tests((current_slot, [(&pubkey3, &zero_lamport_account)].as_slice()));

    let snapshot_stores = accounts.get_storages(..=current_slot).0;
    let total_accounts: usize = snapshot_stores.iter().map(|s| s.accounts_count()).sum();
    assert!(!snapshot_stores.is_empty());
    assert!(total_accounts > 0);

    info!("post D");
    accounts.print_accounts_stats("Post-D");

    accounts.add_root_and_flush_write_cache(current_slot);
    accounts.clean_accounts_for_tests();

    accounts.print_accounts_stats("Post-D clean");

    let total_accounts_post_clean: usize = snapshot_stores.iter().map(|s| s.accounts_count()).sum();
    assert_eq!(total_accounts, total_accounts_post_clean);

    // should clean all 3 pubkeys
    assert_eq!(accounts.ref_count_for_pubkey(&pubkey1), 0);
    assert_eq!(accounts.ref_count_for_pubkey(&pubkey2), 0);
    assert_eq!(accounts.ref_count_for_pubkey(&pubkey3), 0);
}

// Setup 3 scenarios which try to differentiate between pubkey1 being in an
// Available slot or a Full slot which would cause a different reset behavior
// when pubkey1 is cleaned and therefore cause the ref count to be incorrect
// preventing a removal of that key.
//
// do stores with a 4mb size so only 1 store is created per slot
define_accounts_db_test!(test_full_clean_refcount_no_first_4m, |accounts| {
    do_full_clean_refcount(accounts, false, 4 * 1024 * 1024);
});

// do stores with a 4k size and store pubkey1 first
define_accounts_db_test!(test_full_clean_refcount_no_first_4k, |accounts| {
    do_full_clean_refcount(accounts, false, 4 * 1024);
});

// do stores with a 4k size and store pubkey1 2nd
define_accounts_db_test!(test_full_clean_refcount_first_4k, |accounts| {
    do_full_clean_refcount(accounts, true, 4 * 1024);
});

#[test]
fn test_clean_stored_dead_slots_empty() {
    let accounts = AccountsDb::new_single_for_tests();
    let mut dead_slots = IntSet::default();
    dead_slots.insert(10);
    accounts.clean_stored_dead_slots(&dead_slots, None, &HashSet::default());
}

#[test]
fn test_shrink_all_slots_none() {
    let epoch_schedule = EpochSchedule::default();
    for startup in &[false, true] {
        let accounts = AccountsDb::new_single_for_tests();

        for _ in 0..10 {
            accounts.shrink_candidate_slots(&epoch_schedule);
        }

        accounts.shrink_all_slots(*startup, &EpochSchedule::default(), None);
    }
}

#[test]
fn test_shrink_candidate_slots() {
    solana_logger::setup();

    let mut accounts = AccountsDb::new_single_for_tests();

    let pubkey_count = 30000;
    let pubkeys: Vec<_> = (0..pubkey_count)
        .map(|_| solana_pubkey::new_rand())
        .collect();

    let some_lamport = 223;
    let no_data = 0;
    let owner = *AccountSharedData::default().owner();

    let account = AccountSharedData::new(some_lamport, no_data, &owner);

    let mut current_slot = 0;

    current_slot += 1;
    for pubkey in &pubkeys {
        accounts.store_for_tests((current_slot, [(pubkey, &account)].as_slice()));
    }
    let shrink_slot = current_slot;
    accounts.add_root_and_flush_write_cache(current_slot);

    current_slot += 1;
    let pubkey_count_after_shrink = 25000;
    let updated_pubkeys = &pubkeys[0..pubkey_count - pubkey_count_after_shrink];

    for pubkey in updated_pubkeys {
        accounts.store_for_tests((current_slot, [(pubkey, &account)].as_slice()));
    }
    accounts.add_root_and_flush_write_cache(current_slot);
    accounts.clean_accounts_for_tests();

    assert_eq!(
        pubkey_count,
        accounts.all_account_count_in_accounts_file(shrink_slot)
    );

    // Only, try to shrink stale slots, nothing happens because shrink ratio
    // is not small enough to do a shrink
    // Note this shrink ratio had to change because we are WAY over-allocating append vecs when we flush the write cache at the moment.
    accounts.shrink_ratio = AccountShrinkThreshold::TotalSpace { shrink_ratio: 0.4 };
    accounts.shrink_candidate_slots(&EpochSchedule::default());
    assert_eq!(
        pubkey_count,
        accounts.all_account_count_in_accounts_file(shrink_slot)
    );

    // Now, do full-shrink.
    accounts.shrink_all_slots(false, &EpochSchedule::default(), None);
    assert_eq!(
        pubkey_count_after_shrink,
        accounts.all_account_count_in_accounts_file(shrink_slot)
    );
}

/// This test creates an ancient storage with three alive accounts
/// of various sizes. It then simulates killing one of the
/// accounts in a more recent (non-ancient) slot by overwriting
/// the account that has the smallest data size.  The dead account
/// is expected to be deleted from its ancient storage in the
/// process of shrinking candidate slots.  The capacity of the
/// storage after shrinking is expected to be the sum of alive
/// bytes of the two remaining alive ancient accounts.
#[test]
fn test_shrink_candidate_slots_with_dead_ancient_account() {
    solana_logger::setup();
    let epoch_schedule = EpochSchedule::default();
    let db = AccountsDb::new_single_for_tests();
    const ACCOUNT_DATA_SIZES: &[usize] = &[1000, 2000, 150];
    let accounts: Vec<_> = ACCOUNT_DATA_SIZES
        .iter()
        .map(|data_size| {
            (
                Pubkey::new_unique(),
                AccountSharedData::new(1, *data_size, &Pubkey::default()),
            )
        })
        .collect();
    let accounts: Vec<_> = accounts
        .iter()
        .map(|(pubkey, account)| (pubkey, account))
        .collect();
    let starting_ancient_slot = 1;
    db.store_for_tests((starting_ancient_slot, accounts.as_slice()));
    db.add_root_and_flush_write_cache(starting_ancient_slot);
    let storage = db.get_storage_for_slot(starting_ancient_slot).unwrap();
    let ancient_accounts = db.get_unique_accounts_from_storage(&storage);
    // Check that three accounts are indeed present in the combined storage.
    assert_eq!(ancient_accounts.stored_accounts.len(), 3);
    // Find an ancient account with smallest data length.
    // This will be a dead account, overwritten in the current slot.
    let modified_account_pubkey = ancient_accounts
        .stored_accounts
        .iter()
        .min_by(|a, b| a.data_len.cmp(&b.data_len))
        .unwrap()
        .pubkey;
    let modified_account_owner = *AccountSharedData::default().owner();
    let modified_account = AccountSharedData::new(223, 0, &modified_account_owner);
    let ancient_append_vec_offset = db.ancient_append_vec_offset.unwrap().abs();
    let current_slot = epoch_schedule.slots_per_epoch + ancient_append_vec_offset as u64 + 1;
    // Simulate killing of the ancient account by overwriting it in the current slot.
    db.store_for_tests((
        current_slot,
        [(&modified_account_pubkey, &modified_account)].as_slice(),
    ));
    db.add_root_and_flush_write_cache(current_slot);
    // This should remove the dead ancient account from the index.
    db.clean_accounts_for_tests();
    db.shrink_ancient_slots(&epoch_schedule);
    let storage = db.get_storage_for_slot(starting_ancient_slot).unwrap();
    let created_accounts = db.get_unique_accounts_from_storage(&storage);
    // The dead account should still be in the ancient storage,
    // because the storage wouldn't be shrunk with normal alive to
    // capacity ratio.
    assert_eq!(created_accounts.stored_accounts.len(), 3);
    db.shrink_candidate_slots(&epoch_schedule);
    let storage = db.get_storage_for_slot(starting_ancient_slot).unwrap();
    let created_accounts = db.get_unique_accounts_from_storage(&storage);
    // At this point the dead ancient account should be removed
    // and storage capacity shrunk to the sum of alive bytes of
    // accounts it holds.  This is the data lengths of the
    // accounts plus the length of their metadata.
    assert_eq!(
        created_accounts.capacity as usize,
        aligned_stored_size(1000) + aligned_stored_size(2000)
    );
    // The above check works only when the AppendVec storage is
    // used. More generally the pubkey of the smallest account
    // shouldn't be present in the shrunk storage, which is
    // validated by the following scan of the storage accounts.
    storage
        .accounts
        .scan_pubkeys(|pubkey| {
            assert_ne!(pubkey, &modified_account_pubkey);
        })
        .expect("must scan accounts storage");
}

#[test]
fn test_select_candidates_by_total_usage_no_candidates() {
    // no input candidates -- none should be selected
    solana_logger::setup();
    let candidates = ShrinkCandidates::default();
    let db = AccountsDb::new_single_for_tests();

    let (selected_candidates, next_candidates) =
        db.select_candidates_by_total_usage(&candidates, DEFAULT_ACCOUNTS_SHRINK_RATIO);

    assert_eq!(0, selected_candidates.len());
    assert_eq!(0, next_candidates.len());
}

#[test]
fn test_select_candidates_by_total_usage_3_way_split_condition() {
    // three candidates, one selected for shrink, one is put back to the candidate list and one is ignored
    solana_logger::setup();
    let mut candidates = ShrinkCandidates::default();
    let db = AccountsDb::new_single_for_tests();

    let common_store_path = Path::new("");
    let store_file_size = 100;

    let store1_slot = 11;
    let store1 = Arc::new(AccountStorageEntry::new(
        common_store_path,
        store1_slot,
        store1_slot as AccountsFileId,
        store_file_size,
        AccountsFileProvider::AppendVec,
    ));
    db.storage.insert(store1_slot, Arc::clone(&store1));
    store1.alive_bytes.store(0, Ordering::Release);
    candidates.insert(store1_slot);

    let store2_slot = 22;
    let store2 = Arc::new(AccountStorageEntry::new(
        common_store_path,
        store2_slot,
        store2_slot as AccountsFileId,
        store_file_size,
        AccountsFileProvider::AppendVec,
    ));
    db.storage.insert(store2_slot, Arc::clone(&store2));
    store2
        .alive_bytes
        .store(store_file_size as usize / 2, Ordering::Release);
    candidates.insert(store2_slot);

    let store3_slot = 33;
    let store3 = Arc::new(AccountStorageEntry::new(
        common_store_path,
        store3_slot,
        store3_slot as AccountsFileId,
        store_file_size,
        AccountsFileProvider::AppendVec,
    ));
    db.storage.insert(store3_slot, Arc::clone(&store3));
    store3
        .alive_bytes
        .store(store_file_size as usize, Ordering::Release);
    candidates.insert(store3_slot);

    // Set the target alive ratio to 0.6 so that we can just get rid of store1, the remaining two stores
    // alive ratio can be > the target ratio: the actual ratio is 0.75 because of 150 alive bytes / 200 total bytes.
    // The target ratio is also set to larger than store2's alive ratio: 0.5 so that it would be added
    // to the candidates list for next round.
    let target_alive_ratio = 0.6;
    let (selected_candidates, next_candidates) =
        db.select_candidates_by_total_usage(&candidates, target_alive_ratio);
    assert_eq!(1, selected_candidates.len());
    assert!(selected_candidates.contains(&store1_slot));
    assert_eq!(1, next_candidates.len());
    assert!(next_candidates.contains(&store2_slot));
}

#[test]
fn test_select_candidates_by_total_usage_2_way_split_condition() {
    // three candidates, 2 are selected for shrink, one is ignored
    solana_logger::setup();
    let db = AccountsDb::new_single_for_tests();
    let mut candidates = ShrinkCandidates::default();

    let common_store_path = Path::new("");
    let store_file_size = 100;

    let store1_slot = 11;
    let store1 = Arc::new(AccountStorageEntry::new(
        common_store_path,
        store1_slot,
        store1_slot as AccountsFileId,
        store_file_size,
        AccountsFileProvider::AppendVec,
    ));
    db.storage.insert(store1_slot, Arc::clone(&store1));
    store1.alive_bytes.store(0, Ordering::Release);
    candidates.insert(store1_slot);

    let store2_slot = 22;
    let store2 = Arc::new(AccountStorageEntry::new(
        common_store_path,
        store2_slot,
        store2_slot as AccountsFileId,
        store_file_size,
        AccountsFileProvider::AppendVec,
    ));
    db.storage.insert(store2_slot, Arc::clone(&store2));
    store2
        .alive_bytes
        .store(store_file_size as usize / 2, Ordering::Release);
    candidates.insert(store2_slot);

    let store3_slot = 33;
    let store3 = Arc::new(AccountStorageEntry::new(
        common_store_path,
        store3_slot,
        store3_slot as AccountsFileId,
        store_file_size,
        AccountsFileProvider::AppendVec,
    ));
    db.storage.insert(store3_slot, Arc::clone(&store3));
    store3
        .alive_bytes
        .store(store_file_size as usize, Ordering::Release);
    candidates.insert(store3_slot);

    // Set the target ratio to default (0.8), both store1 and store2 must be selected and store3 is ignored.
    let target_alive_ratio = DEFAULT_ACCOUNTS_SHRINK_RATIO;
    let (selected_candidates, next_candidates) =
        db.select_candidates_by_total_usage(&candidates, target_alive_ratio);
    assert_eq!(2, selected_candidates.len());
    assert!(selected_candidates.contains(&store1_slot));
    assert!(selected_candidates.contains(&store2_slot));
    assert_eq!(0, next_candidates.len());
}

#[test]
fn test_select_candidates_by_total_usage_all_clean() {
    // 2 candidates, they must be selected to achieve the target alive ratio
    solana_logger::setup();
    let db = AccountsDb::new_single_for_tests();
    let mut candidates = ShrinkCandidates::default();

    let common_store_path = Path::new("");
    let store_file_size = 100;

    let store1_slot = 11;
    let store1 = Arc::new(AccountStorageEntry::new(
        common_store_path,
        store1_slot,
        store1_slot as AccountsFileId,
        store_file_size,
        AccountsFileProvider::AppendVec,
    ));
    db.storage.insert(store1_slot, Arc::clone(&store1));
    store1
        .alive_bytes
        .store(store_file_size as usize / 4, Ordering::Release);
    candidates.insert(store1_slot);

    let store2_slot = 22;
    let store2 = Arc::new(AccountStorageEntry::new(
        common_store_path,
        store2_slot,
        store2_slot as AccountsFileId,
        store_file_size,
        AccountsFileProvider::AppendVec,
    ));
    db.storage.insert(store2_slot, Arc::clone(&store2));
    store2
        .alive_bytes
        .store(store_file_size as usize / 2, Ordering::Release);
    candidates.insert(store2_slot);

    // Set the target ratio to default (0.8), both stores from the two different slots must be selected.
    let target_alive_ratio = DEFAULT_ACCOUNTS_SHRINK_RATIO;
    let (selected_candidates, next_candidates) =
        db.select_candidates_by_total_usage(&candidates, target_alive_ratio);
    assert_eq!(2, selected_candidates.len());
    assert!(selected_candidates.contains(&store1_slot));
    assert!(selected_candidates.contains(&store2_slot));
    assert_eq!(0, next_candidates.len());
}

#[test]
fn test_delete_dependencies() {
    solana_logger::setup();
    let accounts_index = AccountsIndex::<AccountInfo, AccountInfo>::default_for_tests();
    let key0 = Pubkey::new_from_array([0u8; 32]);
    let key1 = Pubkey::new_from_array([1u8; 32]);
    let key2 = Pubkey::new_from_array([2u8; 32]);
    let info0 = AccountInfo::new(StorageLocation::AppendVec(0, 0), true);
    let info1 = AccountInfo::new(StorageLocation::AppendVec(1, 0), true);
    let info2 = AccountInfo::new(StorageLocation::AppendVec(2, 0), true);
    let info3 = AccountInfo::new(StorageLocation::AppendVec(3, 0), true);
    let mut reclaims = vec![];
    accounts_index.upsert(
        0,
        0,
        &key0,
        &AccountSharedData::default(),
        &AccountSecondaryIndexes::default(),
        info0,
        &mut reclaims,
        UpsertReclaim::IgnoreReclaims,
    );
    accounts_index.upsert(
        1,
        1,
        &key0,
        &AccountSharedData::default(),
        &AccountSecondaryIndexes::default(),
        info1,
        &mut reclaims,
        UpsertReclaim::IgnoreReclaims,
    );
    accounts_index.upsert(
        1,
        1,
        &key1,
        &AccountSharedData::default(),
        &AccountSecondaryIndexes::default(),
        info1,
        &mut reclaims,
        UpsertReclaim::IgnoreReclaims,
    );
    accounts_index.upsert(
        2,
        2,
        &key1,
        &AccountSharedData::default(),
        &AccountSecondaryIndexes::default(),
        info2,
        &mut reclaims,
        UpsertReclaim::IgnoreReclaims,
    );
    accounts_index.upsert(
        2,
        2,
        &key2,
        &AccountSharedData::default(),
        &AccountSecondaryIndexes::default(),
        info2,
        &mut reclaims,
        UpsertReclaim::IgnoreReclaims,
    );
    accounts_index.upsert(
        3,
        3,
        &key2,
        &AccountSharedData::default(),
        &AccountSecondaryIndexes::default(),
        info3,
        &mut reclaims,
        UpsertReclaim::IgnoreReclaims,
    );
    accounts_index.add_root(0);
    accounts_index.add_root(1);
    accounts_index.add_root(2);
    accounts_index.add_root(3);
    let num_bins = accounts_index.bins();
    let mut candidates: Box<_> = std::iter::repeat_with(HashMap::<Pubkey, CleaningInfo>::new)
        .take(num_bins)
        .collect();
    for key in [&key0, &key1, &key2] {
        let index_entry = accounts_index.get_cloned(key).unwrap();
        let rooted_entries = accounts_index
            .get_rooted_entries(index_entry.slot_list.read().unwrap().as_slice(), None);
        let ref_count = index_entry.ref_count();
        let index = accounts_index.bin_calculator.bin_from_pubkey(key);
        let candidates_bin = &mut candidates[index];
        candidates_bin.insert(
            *key,
            CleaningInfo {
                slot_list: rooted_entries,
                ref_count,
                ..Default::default()
            },
        );
    }
    for candidates_bin in candidates.iter() {
        for (
            key,
            CleaningInfo {
                slot_list: list,
                ref_count,
                ..
            },
        ) in candidates_bin.iter()
        {
            info!(" purge {key} ref_count {ref_count} =>");
            for x in list {
                info!("  {x:?}");
            }
        }
    }

    let mut store_counts = HashMap::new();
    store_counts.insert(0, (0, HashSet::from_iter(vec![key0])));
    store_counts.insert(1, (0, HashSet::from_iter(vec![key0, key1])));
    store_counts.insert(2, (0, HashSet::from_iter(vec![key1, key2])));
    store_counts.insert(3, (1, HashSet::from_iter(vec![key2])));
    let accounts = AccountsDb::new_single_for_tests();
    accounts.calc_delete_dependencies(&candidates, &mut store_counts, None);
    let mut stores: Vec<_> = store_counts.keys().cloned().collect();
    stores.sort_unstable();
    for store in &stores {
        info!(
            "store: {:?} : {:?}",
            store,
            store_counts.get(store).unwrap()
        );
    }
    for x in 0..3 {
        // if the store count doesn't exist for this id, then it is implied to be > 0
        assert!(store_counts
            .get(&x)
            .map(|entry| entry.0 >= 1)
            .unwrap_or(true));
    }
}

#[test]
fn test_account_balance_for_capitalization_sysvar() {
    let normal_sysvar =
        solana_account::create_account_for_test(&solana_slot_history::SlotHistory::default());
    assert_eq!(normal_sysvar.lamports(), 1);
}

#[test]
fn test_account_balance_for_capitalization_native_program() {
    let normal_native_program = create_loadable_account_for_test("foo");
    assert_eq!(normal_native_program.lamports(), 1);
}

#[test]
fn test_store_overhead() {
    solana_logger::setup();
    let accounts = AccountsDb::new_single_for_tests();
    let account = AccountSharedData::default();
    let pubkey = solana_pubkey::new_rand();
    accounts.store_for_tests((0, [(&pubkey, &account)].as_slice()));
    accounts.add_root_and_flush_write_cache(0);
    let store = accounts.storage.get_slot_storage_entry(0).unwrap();
    let total_len = store.accounts.len();
    info!("total: {total_len}");
    assert_eq!(total_len, STORE_META_OVERHEAD);
}

#[test]
fn test_store_clean_after_shrink() {
    solana_logger::setup();
    let accounts = AccountsDb::new_single_for_tests();
    let epoch_schedule = EpochSchedule::default();

    let account = AccountSharedData::new(1, 16 * 4096, &Pubkey::default());
    let pubkey1 = solana_pubkey::new_rand();
    accounts.store_for_tests((0, &[(&pubkey1, &account)][..]));

    let pubkey2 = solana_pubkey::new_rand();
    accounts.store_for_tests((0, &[(&pubkey2, &account)][..]));

    let zero_account = AccountSharedData::new(0, 1, &Pubkey::default());
    accounts.store_for_tests((1, &[(&pubkey1, &zero_account)][..]));

    // Add root 0 and flush separately
    accounts.add_root(0);
    accounts.flush_accounts_cache(true, None);

    // clear out the dirty keys
    accounts.clean_accounts_for_tests();

    // flush 1
    accounts.add_root(1);
    accounts.flush_accounts_cache(true, None);

    accounts.print_accounts_stats("pre-clean");

    // clean to remove pubkey1 from 0,
    // shrink to shrink pubkey1 from 0
    // then another clean to remove pubkey1 from slot 1
    accounts.clean_accounts_for_tests();

    accounts.shrink_candidate_slots(&epoch_schedule);

    accounts.clean_accounts_for_tests();

    accounts.print_accounts_stats("post-clean");
    assert_eq!(accounts.accounts_index.ref_count_from_storage(&pubkey1), 0);
}

#[test]
#[should_panic(expected = "We've run out of storage ids!")]
fn test_wrapping_storage_id() {
    let db = AccountsDb::new_single_for_tests();

    let zero_lamport_account = AccountSharedData::new(0, 0, AccountSharedData::default().owner());

    // set 'next' id to the max possible value
    db.next_id.store(AccountsFileId::MAX, Ordering::Release);
    let slots = 3;
    let keys = (0..slots).map(|_| Pubkey::new_unique()).collect::<Vec<_>>();
    // write unique keys to successive slots
    keys.iter().enumerate().for_each(|(slot, key)| {
        let slot = slot as Slot;
        db.store_for_tests((slot, [(key, &zero_lamport_account)].as_slice()));
        db.add_root_and_flush_write_cache(slot);
    });
    assert_eq!(slots - 1, db.next_id.load(Ordering::Acquire));
    let ancestors = Ancestors::default();
    keys.iter().for_each(|key| {
        assert!(db.load_without_fixed_root(&ancestors, key).is_some());
    });
}

#[test]
#[should_panic(expected = "We've run out of storage ids!")]
fn test_reuse_storage_id() {
    solana_logger::setup();
    let db = AccountsDb::new_single_for_tests();

    let zero_lamport_account = AccountSharedData::new(0, 0, AccountSharedData::default().owner());

    // set 'next' id to the max possible value
    db.next_id.store(AccountsFileId::MAX, Ordering::Release);
    let slots = 3;
    let keys = (0..slots).map(|_| Pubkey::new_unique()).collect::<Vec<_>>();
    // write unique keys to successive slots
    keys.iter().enumerate().for_each(|(slot, key)| {
        let slot = slot as Slot;
        db.store_for_tests((slot, [(key, &zero_lamport_account)].as_slice()));
        db.add_root_and_flush_write_cache(slot);
        // reset next_id to what it was previously to cause us to re-use the same id
        db.next_id.store(AccountsFileId::MAX, Ordering::Release);
    });
    let ancestors = Ancestors::default();
    keys.iter().for_each(|key| {
        assert!(db.load_without_fixed_root(&ancestors, key).is_some());
    });
}

#[test]
fn test_zero_lamport_new_root_not_cleaned() {
    let db = AccountsDb::new_single_for_tests();
    let account_key = Pubkey::new_unique();
    let zero_lamport_account = AccountSharedData::new(0, 0, AccountSharedData::default().owner());

    // Store zero lamport account into slots 0 and 1, root both slots
    db.store_for_tests((0, [(&account_key, &zero_lamport_account)].as_slice()));
    db.store_for_tests((1, [(&account_key, &zero_lamport_account)].as_slice()));
    db.add_root_and_flush_write_cache(0);
    db.add_root_and_flush_write_cache(1);

    // Only clean zero lamport accounts up to slot 0
    db.clean_accounts(Some(0), false, &EpochSchedule::default());

    // Should still be able to find zero lamport account in slot 1
    assert_eq!(
        db.load_without_fixed_root(&Ancestors::default(), &account_key),
        Some((zero_lamport_account, 1))
    );
}

#[test]
fn test_store_load_cached() {
    let db = AccountsDb::new_single_for_tests();
    let key = Pubkey::default();
    let account0 = AccountSharedData::new(1, 0, &key);
    let slot = 0;
    db.store_for_tests((slot, &[(&key, &account0)][..]));

    // Load with no ancestors and no root will return nothing
    assert!(db
        .load_without_fixed_root(&Ancestors::default(), &key)
        .is_none());

    // Load with ancestors not equal to `slot` will return nothing
    let ancestors = vec![(slot + 1, 1)].into_iter().collect();
    assert!(db.load_without_fixed_root(&ancestors, &key).is_none());

    // Load with ancestors equal to `slot` will return the account
    let ancestors = vec![(slot, 1)].into_iter().collect();
    assert_eq!(
        db.load_without_fixed_root(&ancestors, &key),
        Some((account0.clone(), slot))
    );

    // Adding root will return the account even without ancestors
    db.add_root(slot);
    assert_eq!(
        db.load_without_fixed_root(&Ancestors::default(), &key),
        Some((account0, slot))
    );
}

#[test]
fn test_store_flush_load_cached() {
    let db = AccountsDb::new_single_for_tests();
    let key = Pubkey::default();
    let account0 = AccountSharedData::new(1, 0, &key);
    let slot = 0;
    db.store_for_tests((slot, &[(&key, &account0)][..]));
    db.mark_slot_frozen(slot);

    // No root was added yet, requires an ancestor to find
    // the account
    db.flush_accounts_cache(true, None);
    let ancestors = vec![(slot, 1)].into_iter().collect();
    assert_eq!(
        db.load_without_fixed_root(&ancestors, &key),
        Some((account0.clone(), slot))
    );

    // Add root then flush
    db.add_root(slot);
    db.flush_accounts_cache(true, None);
    assert_eq!(
        db.load_without_fixed_root(&Ancestors::default(), &key),
        Some((account0, slot))
    );
}

#[test]
fn test_flush_accounts_cache() {
    let db = AccountsDb::new_single_for_tests();
    let account0 = AccountSharedData::new(1, 0, &Pubkey::default());

    let unrooted_slot = 4;
    let root5 = 5;
    let root6 = 6;
    let unrooted_key = solana_pubkey::new_rand();
    let key5 = solana_pubkey::new_rand();
    let key6 = solana_pubkey::new_rand();
    db.store_for_tests((unrooted_slot, &[(&unrooted_key, &account0)][..]));
    db.store_for_tests((root5, &[(&key5, &account0)][..]));
    db.store_for_tests((root6, &[(&key6, &account0)][..]));
    for slot in &[unrooted_slot, root5, root6] {
        db.mark_slot_frozen(*slot);
    }
    db.add_root(root5);
    db.add_root(root6);

    // Unrooted slot should be able to be fetched before the flush
    let ancestors = vec![(unrooted_slot, 1)].into_iter().collect();
    assert_eq!(
        db.load_without_fixed_root(&ancestors, &unrooted_key),
        Some((account0.clone(), unrooted_slot))
    );
    db.flush_accounts_cache(true, None);

    // After the flush, the unrooted slot is still in the cache
    assert!(db
        .load_without_fixed_root(&ancestors, &unrooted_key)
        .is_some());
    assert!(db.accounts_index.contains(&unrooted_key));
    assert_eq!(db.accounts_cache.num_slots(), 1);
    assert!(db.accounts_cache.slot_cache(unrooted_slot).is_some());
    assert_eq!(
        db.load_without_fixed_root(&Ancestors::default(), &key5),
        Some((account0.clone(), root5))
    );
    assert_eq!(
        db.load_without_fixed_root(&Ancestors::default(), &key6),
        Some((account0, root6))
    );
}

fn max_cache_slots() -> usize {
    // this used to be the limiting factor - used here to facilitate tests.
    200
}

#[test]
fn test_flush_accounts_cache_if_needed() {
    run_test_flush_accounts_cache_if_needed(0, 2 * max_cache_slots());
    run_test_flush_accounts_cache_if_needed(2 * max_cache_slots(), 0);
    run_test_flush_accounts_cache_if_needed(max_cache_slots() - 1, 0);
    run_test_flush_accounts_cache_if_needed(0, max_cache_slots() - 1);
    run_test_flush_accounts_cache_if_needed(max_cache_slots(), 0);
    run_test_flush_accounts_cache_if_needed(0, max_cache_slots());
    run_test_flush_accounts_cache_if_needed(2 * max_cache_slots(), 2 * max_cache_slots());
    run_test_flush_accounts_cache_if_needed(max_cache_slots() - 1, max_cache_slots() - 1);
    run_test_flush_accounts_cache_if_needed(max_cache_slots(), max_cache_slots());
}

fn run_test_flush_accounts_cache_if_needed(num_roots: usize, num_unrooted: usize) {
    let mut db = AccountsDb::new_single_for_tests();
    db.write_cache_limit_bytes = Some(max_cache_slots() as u64);
    let space = 1; // # data bytes per account. write cache counts data len
    let account0 = AccountSharedData::new(1, space, &Pubkey::default());
    let mut keys = vec![];
    let num_slots = 2 * max_cache_slots();
    for i in 0..num_roots + num_unrooted {
        let key = Pubkey::new_unique();
        db.store_for_tests((i as Slot, &[(&key, &account0)][..]));
        keys.push(key);
        db.mark_slot_frozen(i as Slot);
        if i < num_roots {
            db.add_root(i as Slot);
        }
    }

    db.flush_accounts_cache(false, None);

    let total_slots = num_roots + num_unrooted;
    // If there's <= the max size, then nothing will be flushed from the slot
    if total_slots <= max_cache_slots() {
        assert_eq!(db.accounts_cache.num_slots(), total_slots);
    } else {
        // Otherwise, all the roots are flushed, and only at most max_cache_slots()
        // of the unrooted slots are kept in the cache
        let expected_size = std::cmp::min(num_unrooted, max_cache_slots());
        if expected_size > 0 {
            // +1: slot is 1-based. slot 1 has 1 byte of data
            for unrooted_slot in (total_slots - expected_size + 1)..total_slots {
                assert!(
                    db.accounts_cache
                        .slot_cache(unrooted_slot as Slot)
                        .is_some(),
                    "unrooted_slot: {unrooted_slot}, total_slots: {total_slots}, expected_size: \
                     {expected_size}"
                );
            }
        }
    }

    // Should still be able to fetch all the accounts after flush
    for (slot, key) in (0..num_slots as Slot).zip(keys) {
        let ancestors = if slot < num_roots as Slot {
            Ancestors::default()
        } else {
            vec![(slot, 1)].into_iter().collect()
        };
        assert_eq!(
            db.load_without_fixed_root(&ancestors, &key),
            Some((account0.clone(), slot))
        );
    }
}

#[test]
fn test_read_only_accounts_cache() {
    let db = Arc::new(AccountsDb::new_single_for_tests());

    let account_key = Pubkey::new_unique();
    let zero_lamport_account = AccountSharedData::new(0, 0, AccountSharedData::default().owner());
    let slot1_account = AccountSharedData::new(1, 1, AccountSharedData::default().owner());
    db.store_for_tests((0, &[(&account_key, &zero_lamport_account)][..]));
    db.store_for_tests((1, &[(&account_key, &slot1_account)][..]));

    db.add_root(0);
    db.add_root(1);
    db.clean_accounts_for_tests();
    db.flush_accounts_cache(true, None);
    db.clean_accounts_for_tests();
    db.add_root(2);

    assert_eq!(db.read_only_accounts_cache.cache_len(), 0);
    let account = db
        .load_with_fixed_root(&Ancestors::default(), &account_key)
        .map(|(account, _)| account)
        .unwrap();
    assert_eq!(account.lamports(), 1);
    assert_eq!(db.read_only_accounts_cache.cache_len(), 1);
    let account = db
        .load_with_fixed_root(&Ancestors::default(), &account_key)
        .map(|(account, _)| account)
        .unwrap();
    assert_eq!(account.lamports(), 1);
    assert_eq!(db.read_only_accounts_cache.cache_len(), 1);
    db.store_for_tests((2, &[(&account_key, &zero_lamport_account)][..]));
    assert_eq!(db.read_only_accounts_cache.cache_len(), 1);
    let account = db
        .load_with_fixed_root(&Ancestors::default(), &account_key)
        .map(|(account, _)| account);
    assert!(account.is_none());
    assert_eq!(db.read_only_accounts_cache.cache_len(), 1);
}

#[test]
fn test_load_with_read_only_accounts_cache() {
    let db = Arc::new(AccountsDb::new_single_for_tests());

    let account_key = Pubkey::new_unique();
    let zero_lamport_account = AccountSharedData::new(0, 0, AccountSharedData::default().owner());
    let slot1_account = AccountSharedData::new(1, 1, AccountSharedData::default().owner());
    db.store_for_tests((0, &[(&account_key, &zero_lamport_account)][..]));
    db.store_for_tests((1, &[(&account_key, &slot1_account)][..]));

    db.add_root(0);
    db.add_root(1);
    db.clean_accounts_for_tests();
    db.flush_accounts_cache(true, None);
    db.clean_accounts_for_tests();
    db.add_root(2);

    assert_eq!(db.read_only_accounts_cache.cache_len(), 0);
    let (account, slot) = db
        .load_account_with(&Ancestors::default(), &account_key, false)
        .unwrap();
    assert_eq!(account.lamports(), 1);
    assert_eq!(db.read_only_accounts_cache.cache_len(), 0);
    assert_eq!(slot, 1);

    let (account, slot) = db
        .load_account_with(&Ancestors::default(), &account_key, true)
        .unwrap();
    assert_eq!(account.lamports(), 1);
    assert_eq!(db.read_only_accounts_cache.cache_len(), 1);
    assert_eq!(slot, 1);

    db.store_for_tests((2, &[(&account_key, &zero_lamport_account)][..]));
    let account = db.load_account_with(&Ancestors::default(), &account_key, false);
    assert!(account.is_none());
    assert_eq!(db.read_only_accounts_cache.cache_len(), 1);

    db.read_only_accounts_cache.reset_for_tests();
    assert_eq!(db.read_only_accounts_cache.cache_len(), 0);
    let account = db.load_account_with(&Ancestors::default(), &account_key, true);
    assert!(account.is_none());
    assert_eq!(db.read_only_accounts_cache.cache_len(), 0);

    let slot2_account = AccountSharedData::new(2, 1, AccountSharedData::default().owner());
    db.store_for_tests((2, &[(&account_key, &slot2_account)][..]));
    let (account, slot) = db
        .load_account_with(&Ancestors::default(), &account_key, false)
        .unwrap();
    assert_eq!(account.lamports(), 2);
    assert_eq!(db.read_only_accounts_cache.cache_len(), 0);
    assert_eq!(slot, 2);

    let slot2_account = AccountSharedData::new(2, 1, AccountSharedData::default().owner());
    db.store_for_tests((2, &[(&account_key, &slot2_account)][..]));
    let (account, slot) = db
        .load_account_with(&Ancestors::default(), &account_key, true)
        .unwrap();
    assert_eq!(account.lamports(), 2);
    // The account shouldn't be added to read_only_cache because it is in write_cache.
    assert_eq!(db.read_only_accounts_cache.cache_len(), 0);
    assert_eq!(slot, 2);
}

/// a test that will accept either answer
const LOAD_ZERO_LAMPORTS_ANY_TESTS: LoadZeroLamports = LoadZeroLamports::None;

#[test]
fn test_flush_cache_clean() {
    let db = Arc::new(AccountsDb::new_single_for_tests());

    let account_key = Pubkey::new_unique();
    let zero_lamport_account = AccountSharedData::new(0, 0, AccountSharedData::default().owner());
    let slot1_account = AccountSharedData::new(1, 1, AccountSharedData::default().owner());
    db.store_for_tests((0, &[(&account_key, &zero_lamport_account)][..]));
    db.store_for_tests((1, &[(&account_key, &slot1_account)][..]));

    db.add_root(0);
    db.add_root(1);

    // Clean should not remove anything yet as nothing has been flushed
    db.clean_accounts_for_tests();
    let account = db
        .do_load(
            &Ancestors::default(),
            &account_key,
            Some(0),
            LoadHint::Unspecified,
            LoadZeroLamports::SomeWithZeroLamportAccountForTests,
        )
        .unwrap();
    assert_eq!(account.0.lamports(), 0);
    // since this item is in the cache, it should not be in the read only cache
    assert_eq!(db.read_only_accounts_cache.cache_len(), 0);

    // Flush, then clean again. Should not need another root to initiate the cleaning
    // because `accounts_index.uncleaned_roots` should be correct
    db.flush_accounts_cache(true, None);
    db.clean_accounts_for_tests();
    assert!(db
        .do_load(
            &Ancestors::default(),
            &account_key,
            Some(0),
            LoadHint::Unspecified,
            LOAD_ZERO_LAMPORTS_ANY_TESTS
        )
        .is_none());
}

#[test]
fn test_flush_cache_dont_clean_zero_lamport_account() {
    let db = Arc::new(AccountsDb::new_single_for_tests());

    let zero_lamport_account_key = Pubkey::new_unique();
    let other_account_key = Pubkey::new_unique();

    let original_lamports = 1;
    let slot0_account =
        AccountSharedData::new(original_lamports, 1, AccountSharedData::default().owner());
    let zero_lamport_account = AccountSharedData::new(0, 0, AccountSharedData::default().owner());

    // Store into slot 0, and then flush the slot to storage
    db.store_for_tests((0, &[(&zero_lamport_account_key, &slot0_account)][..]));
    // Second key keeps other lamport account entry for slot 0 alive,
    // preventing clean of the zero_lamport_account in slot 1.
    db.store_for_tests((0, &[(&other_account_key, &slot0_account)][..]));
    db.add_root(0);
    db.flush_accounts_cache(true, None);
    assert!(db.storage.get_slot_storage_entry(0).is_some());

    // Store into slot 1, a dummy slot that will be dead and purged before flush
    db.store_for_tests((1, &[(&zero_lamport_account_key, &zero_lamport_account)][..]));

    // Store into slot 2, which makes all updates from slot 1 outdated.
    // This means slot 1 is a dead slot. Later, slot 1 will be cleaned/purged
    // before it even reaches storage, but this purge of slot 1should not affect
    // the refcount of `zero_lamport_account_key` because cached keys do not bump
    // the refcount in the index. This means clean should *not* remove
    // `zero_lamport_account_key` from slot 2
    db.store_for_tests((2, &[(&zero_lamport_account_key, &zero_lamport_account)][..]));
    db.add_root(1);
    db.add_root(2);

    // Flush, then clean. Should not need another root to initiate the cleaning
    // because `accounts_index.uncleaned_roots` should be correct
    db.flush_accounts_cache(true, None);
    db.clean_accounts_for_tests();

    // The `zero_lamport_account_key` is still alive in slot 1, so refcount for the
    // pubkey should be 2
    assert_eq!(
        db.accounts_index
            .ref_count_from_storage(&zero_lamport_account_key),
        2
    );
    assert_eq!(
        db.accounts_index.ref_count_from_storage(&other_account_key),
        1
    );

    // The zero-lamport account in slot 2 should not be purged yet, because the
    // entry in slot 1 is blocking cleanup of the zero-lamport account.
    let max_root = None;
    // Fine to simulate a transaction load since we are not doing any out of band
    // removals, only using clean_accounts
    let load_hint = LoadHint::FixedMaxRoot;
    assert_eq!(
        db.do_load(
            &Ancestors::default(),
            &zero_lamport_account_key,
            max_root,
            load_hint,
            LoadZeroLamports::SomeWithZeroLamportAccountForTests,
        )
        .unwrap()
        .0
        .lamports(),
        0
    );
}

/// Ensure that rooting a slot and flushing it in the write cache populates `uncleaned_pubkeys`,
/// and then that `clean` removes the slot afterwards.
#[test]
fn test_flush_cache_populates_uncleaned_pubkeys() {
    let accounts_db = AccountsDb::new_single_for_tests();
    let slot = 123;
    let pubkey = Pubkey::new_unique();
    let account = AccountSharedData::new(10, 0, &Pubkey::default());

    // storing accounts doesn't add anything to uncleaned_pubkeys
    accounts_db.store_for_tests((slot, [(pubkey, account)].as_slice()));
    assert_eq!(accounts_db.get_len_of_slots_with_uncleaned_pubkeys(), 0);

    // ...but ensure that rooting and flushing the write cache does
    accounts_db.add_root_and_flush_write_cache(slot);
    assert_eq!(accounts_db.get_len_of_slots_with_uncleaned_pubkeys(), 1);

    // ...and then clean removes the slot from uncleaned_pubkeys
    accounts_db.clean_accounts_for_tests();
    assert_eq!(accounts_db.get_len_of_slots_with_uncleaned_pubkeys(), 0);
}

struct ScanTracker {
    t_scan: JoinHandle<()>,
    exit: Arc<AtomicBool>,
}

impl ScanTracker {
    fn exit(self) -> thread::Result<()> {
        self.exit.store(true, Ordering::Relaxed);
        self.t_scan.join()
    }
}

fn setup_scan(
    db: Arc<AccountsDb>,
    scan_ancestors: Arc<Ancestors>,
    bank_id: BankId,
    stall_key: Pubkey,
) -> ScanTracker {
    let exit = Arc::new(AtomicBool::new(false));
    let exit_ = exit.clone();
    let ready = Arc::new(AtomicBool::new(false));
    let ready_ = ready.clone();

    let t_scan = Builder::new()
        .name("scan".to_string())
        .spawn(move || {
            db.scan_accounts(
                &scan_ancestors,
                bank_id,
                |maybe_account| {
                    ready_.store(true, Ordering::Relaxed);
                    if let Some((pubkey, _, _)) = maybe_account {
                        if *pubkey == stall_key {
                            loop {
                                if exit_.load(Ordering::Relaxed) {
                                    break;
                                } else {
                                    sleep(Duration::from_millis(10));
                                }
                            }
                        }
                    }
                },
                &ScanConfig::default(),
            )
            .unwrap();
        })
        .unwrap();

    // Wait for scan to start
    while !ready.load(Ordering::Relaxed) {
        sleep(Duration::from_millis(10));
    }

    ScanTracker { t_scan, exit }
}

#[test]
fn test_scan_flush_accounts_cache_then_clean_drop() {
    let db = Arc::new(AccountsDb::new_single_for_tests());
    let account_key = Pubkey::new_unique();
    let account_key2 = Pubkey::new_unique();
    let zero_lamport_account = AccountSharedData::new(0, 0, AccountSharedData::default().owner());
    let slot1_account = AccountSharedData::new(1, 1, AccountSharedData::default().owner());
    let slot2_account = AccountSharedData::new(2, 1, AccountSharedData::default().owner());

    /*
        Store zero lamport account into slots 0, 1, 2 where
        root slots are 0, 2, and slot 1 is unrooted.
                                0 (root)
                            /        \
                          1            2 (root)
    */
    db.store_for_tests((0, &[(&account_key, &zero_lamport_account)][..]));
    db.store_for_tests((1, &[(&account_key, &slot1_account)][..]));
    // Fodder for the scan so that the lock on `account_key` is not held
    db.store_for_tests((1, &[(&account_key2, &slot1_account)][..]));
    db.store_for_tests((2, &[(&account_key, &slot2_account)][..]));

    let max_scan_root = 0;
    db.add_root(max_scan_root);
    let scan_ancestors: Arc<Ancestors> = Arc::new(vec![(0, 1), (1, 1)].into_iter().collect());
    let bank_id = 0;
    let scan_tracker = setup_scan(db.clone(), scan_ancestors.clone(), bank_id, account_key2);

    // Add a new root 2
    let new_root = 2;
    db.add_root(new_root);

    // Check that the scan is properly set up
    assert_eq!(
        db.accounts_index.min_ongoing_scan_root().unwrap(),
        max_scan_root
    );

    // If we specify a requested_flush_root == 2, then `slot 2 <= max_flush_slot` will
    // be flushed even though `slot 2 > max_scan_root`. The unrooted slot 1 should
    // remain in the cache
    db.flush_accounts_cache(true, Some(new_root));
    assert_eq!(db.accounts_cache.num_slots(), 1);
    assert!(db.accounts_cache.slot_cache(1).is_some());

    // Intra cache cleaning should not clean the entry for `account_key` from slot 0,
    // even though it was updated in slot `2` because of the ongoing scan
    let account = db
        .do_load(
            &Ancestors::default(),
            &account_key,
            Some(0),
            LoadHint::Unspecified,
            LoadZeroLamports::SomeWithZeroLamportAccountForTests,
        )
        .unwrap();
    assert_eq!(account.0.lamports(), zero_lamport_account.lamports());

    // Run clean, unrooted slot 1 should not be purged, and still readable from the cache,
    // because we're still doing a scan on it.
    db.clean_accounts_for_tests();
    let account = db
        .do_load(
            &scan_ancestors,
            &account_key,
            Some(max_scan_root),
            LoadHint::Unspecified,
            LOAD_ZERO_LAMPORTS_ANY_TESTS,
        )
        .unwrap();
    assert_eq!(account.0.lamports(), slot1_account.lamports());

    // When the scan is over, clean should not panic and should not purge something
    // still in the cache.
    scan_tracker.exit().unwrap();
    db.clean_accounts_for_tests();
    let account = db
        .do_load(
            &scan_ancestors,
            &account_key,
            Some(max_scan_root),
            LoadHint::Unspecified,
            LOAD_ZERO_LAMPORTS_ANY_TESTS,
        )
        .unwrap();
    assert_eq!(account.0.lamports(), slot1_account.lamports());

    // Simulate dropping the bank, which finally removes the slot from the cache
    let bank_id = 1;
    db.purge_slot(1, bank_id, false);
    assert!(db
        .do_load(
            &scan_ancestors,
            &account_key,
            Some(max_scan_root),
            LoadHint::Unspecified,
            LOAD_ZERO_LAMPORTS_ANY_TESTS
        )
        .is_none());
}

impl AccountsDb {
    fn get_and_assert_single_storage(&self, slot: Slot) -> Arc<AccountStorageEntry> {
        self.storage.get_slot_storage_entry(slot).unwrap()
    }
}

define_accounts_db_test!(test_alive_bytes, |accounts_db| {
    let slot: Slot = 0;
    let num_keys = 10;
    let mut num_obsolete_accounts = 0;

    for data_size in 0..num_keys {
        let account = AccountSharedData::new(1, data_size, &Pubkey::default());
        accounts_db.store_for_tests((slot, &[(&Pubkey::new_unique(), &account)][..]));
    }

    accounts_db.add_root(slot);
    accounts_db.flush_accounts_cache(true, None);

    // Flushing cache should only create one storage entry
    let storage0 = accounts_db.get_and_assert_single_storage(slot);

    storage0
        .accounts
        .scan_accounts_without_data(|_offset, account| {
            let before_size = storage0.alive_bytes();
            let account_info = accounts_db
                .accounts_index
                .get_cloned(account.pubkey())
                .unwrap()
                .slot_list
                .read()
                .unwrap()
                // Should only be one entry per key, since every key was only stored to slot 0
                [0];
            assert_eq!(account_info.0, slot);
            let reclaims = [account_info];
            num_obsolete_accounts += reclaims.len();
            accounts_db.remove_dead_accounts(
                reclaims.iter(),
                None,
                MarkAccountsObsolete::Yes(slot),
            );
            let after_size = storage0.alive_bytes();
            if storage0.count() == 0 {
                // when `remove_dead_accounts` reaches 0 accounts, all bytes are marked as dead
                assert_eq!(after_size, 0);
            } else {
                let stored_size_aligned = storage0.accounts.calculate_stored_size(account.data_len);
                assert_eq!(before_size, after_size + stored_size_aligned);
                assert_eq!(
                    storage0.get_obsolete_accounts(None).len(),
                    num_obsolete_accounts
                );
            }
        })
        .expect("must scan accounts storage");
});

// Test alive_bytes_exclude_zero_lamport_single_ref_accounts calculation
define_accounts_db_test!(
    test_alive_bytes_exclude_zero_lamport_single_ref_accounts,
    |accounts_db| {
        let slot: Slot = 0;
        let num_keys = 10;
        let mut pubkeys = vec![];

        // populate storage with zero lamport single ref (zlsr) accounts
        for _i in 0..num_keys {
            let zero_account = AccountSharedData::new(0, 0, AccountSharedData::default().owner());

            let key = Pubkey::new_unique();
            accounts_db.store_for_tests((slot, &[(&key, &zero_account)][..]));
            pubkeys.push(key);
        }

        accounts_db.add_root(slot);
        accounts_db.flush_accounts_cache(true, None);

        // Flushing cache should only create one storage entry
        let storage = accounts_db.get_and_assert_single_storage(slot);
        let alive_bytes = storage.alive_bytes();
        assert!(alive_bytes > 0);

        // scan the accounts to track zlsr accounts
        accounts_db.accounts_index.scan(
            pubkeys.iter(),
            |_pubkey, slots_refs, _entry| {
                let (slot_list, ref_count) = slots_refs.unwrap();
                assert_eq!(slot_list.len(), 1);
                assert_eq!(ref_count, 1);

                let (slot, acct_info) = slot_list.first().unwrap();
                assert_eq!(*slot, 0);
                accounts_db.zero_lamport_single_ref_found(*slot, acct_info.offset());
                AccountsIndexScanResult::OnlyKeepInMemoryIfDirty
            },
            None,
            false,
            ScanFilter::All,
        );

        // assert the number of zlsr accounts
        assert_eq!(storage.num_zero_lamport_single_ref_accounts(), num_keys);

        // assert the "alive_bytes_exclude_zero_lamport_single_ref_accounts"
        match accounts_db.accounts_file_provider {
            AccountsFileProvider::AppendVec => {
                assert_eq!(
                    storage.alive_bytes_exclude_zero_lamport_single_ref_accounts(),
                    0
                );
            }
            AccountsFileProvider::HotStorage => {
                // For tired-storage, alive bytes are only an approximation.
                // Therefore, it won't be zero.
                assert!(
                    storage.alive_bytes_exclude_zero_lamport_single_ref_accounts() < alive_bytes
                );
            }
        }
    }
);

fn setup_accounts_db_cache_clean(
    num_slots: usize,
    scan_slot: Option<Slot>,
    write_cache_limit_bytes: Option<u64>,
) -> (Arc<AccountsDb>, Vec<Pubkey>, Vec<Slot>, Option<ScanTracker>) {
    let mut accounts_db = AccountsDb::new_single_for_tests();
    accounts_db.write_cache_limit_bytes = write_cache_limit_bytes;
    let accounts_db = Arc::new(accounts_db);

    let slots: Vec<_> = (0..num_slots as Slot).collect();
    let stall_slot = num_slots as Slot;
    let scan_stall_key = Pubkey::new_unique();
    let keys: Vec<Pubkey> = std::iter::repeat_with(Pubkey::new_unique)
        .take(num_slots)
        .collect();
    if scan_slot.is_some() {
        accounts_db.store_for_tests(
            // Store it in a slot that isn't returned in `slots`
            (
                stall_slot,
                &[(
                    &scan_stall_key,
                    &AccountSharedData::new(1, 0, &Pubkey::default()),
                )][..],
            ),
        );
    }

    // Store some subset of the keys in slots 0..num_slots
    let mut scan_tracker = None;
    for slot in &slots {
        for key in &keys[*slot as usize..] {
            let space = 1; // 1 byte allows us to track by size
            accounts_db.store_for_tests((
                *slot,
                &[(key, &AccountSharedData::new(1, space, &Pubkey::default()))][..],
            ));
        }
        accounts_db.add_root(*slot as Slot);
        if Some(*slot) == scan_slot {
            let ancestors = Arc::new(vec![(stall_slot, 1), (*slot, 1)].into_iter().collect());
            let bank_id = 0;
            scan_tracker = Some(setup_scan(
                accounts_db.clone(),
                ancestors,
                bank_id,
                scan_stall_key,
            ));
            assert_eq!(
                accounts_db.accounts_index.min_ongoing_scan_root().unwrap(),
                *slot
            );
        }
    }

    accounts_db.accounts_cache.remove_slot(stall_slot);

    // If there's <= max_cache_slots(), no slots should be flushed
    if accounts_db.accounts_cache.num_slots() <= max_cache_slots() {
        accounts_db.flush_accounts_cache(false, None);
        assert_eq!(accounts_db.accounts_cache.num_slots(), num_slots);
    }

    (accounts_db, keys, slots, scan_tracker)
}

#[test]
fn test_accounts_db_cache_clean_dead_slots() {
    let num_slots = 10;
    let (accounts_db, keys, mut slots, _) = setup_accounts_db_cache_clean(num_slots, None, None);
    let last_dead_slot = (num_slots - 1) as Slot;
    assert_eq!(*slots.last().unwrap(), last_dead_slot);
    let alive_slot = last_dead_slot as Slot + 1;
    slots.push(alive_slot);
    for key in &keys {
        // Store a slot that overwrites all previous keys, rendering all previous keys dead
        accounts_db.store_for_tests((
            alive_slot,
            &[(key, &AccountSharedData::new(1, 0, &Pubkey::default()))][..],
        ));
        accounts_db.add_root(alive_slot);
    }

    // Before the flush, we can find entries in the database for slots < alive_slot if we specify
    // a smaller max root
    for key in &keys {
        assert!(accounts_db
            .do_load(
                &Ancestors::default(),
                key,
                Some(last_dead_slot),
                LoadHint::Unspecified,
                LOAD_ZERO_LAMPORTS_ANY_TESTS
            )
            .is_some());
    }

    // If no `max_clean_root` is specified, cleaning should purge all flushed slots
    accounts_db.flush_accounts_cache(true, None);
    assert_eq!(accounts_db.accounts_cache.num_slots(), 0);
    assert_eq!(
        accounts_db.accounts_cache.fetch_max_flush_root(),
        alive_slot,
    );

    // Specifying a max_root < alive_slot, should not return any more entries,
    // as those have been purged from the accounts index for the dead slots.
    for key in &keys {
        assert!(accounts_db
            .do_load(
                &Ancestors::default(),
                key,
                Some(last_dead_slot),
                LoadHint::Unspecified,
                LOAD_ZERO_LAMPORTS_ANY_TESTS
            )
            .is_none());
    }
    // Each slot should only have one entry in the storage, since all other accounts were
    // cleaned due to later updates
    for slot in &slots {
        if let ScanStorageResult::Stored(slot_accounts) = accounts_db.scan_account_storage(
            *slot as Slot,
            |_| Some(0),
            |slot_accounts: &mut HashSet<Pubkey>, stored_account, _data| {
                slot_accounts.insert(*stored_account.pubkey());
            },
            ScanAccountStorageData::NoData,
        ) {
            if *slot == alive_slot {
                assert_eq!(slot_accounts.len(), keys.len());
            } else {
                assert!(slot_accounts.is_empty());
            }
        } else {
            panic!("Expected slot to be in storage, not cache");
        }
    }
}

#[test]
fn test_accounts_db_cache_clean() {
    let (accounts_db, keys, slots, _) = setup_accounts_db_cache_clean(10, None, None);

    // If no `max_clean_root` is specified, cleaning should purge all flushed slots
    accounts_db.flush_accounts_cache(true, None);
    assert_eq!(accounts_db.accounts_cache.num_slots(), 0);
    assert_eq!(
        accounts_db.accounts_cache.fetch_max_flush_root(),
        *slots.last().unwrap()
    );

    // Each slot should only have one entry in the storage, since all other accounts were
    // cleaned due to later updates
    for slot in &slots {
        if let ScanStorageResult::Stored(slot_account) = accounts_db.scan_account_storage(
            *slot as Slot,
            |_| Some(0),
            |slot_account: &mut Pubkey, stored_account, _data| {
                *slot_account = *stored_account.pubkey();
            },
            ScanAccountStorageData::NoData,
        ) {
            assert_eq!(slot_account, keys[*slot as usize]);
        } else {
            panic!("Everything should have been flushed")
        }
    }
}

fn run_test_accounts_db_cache_clean_max_root(
    num_slots: usize,
    requested_flush_root: Slot,
    scan_root: Option<Slot>,
) {
    assert!(requested_flush_root < (num_slots as Slot));
    let (accounts_db, keys, slots, scan_tracker) =
        setup_accounts_db_cache_clean(num_slots, scan_root, Some(max_cache_slots() as u64));
    let is_cache_at_limit = num_slots - requested_flush_root as usize - 1 > max_cache_slots();

    // If:
    // 1) `requested_flush_root` is specified,
    // 2) not at the cache limit, i.e. `is_cache_at_limit == false`, then
    // `flush_accounts_cache()` should clean and flush only slots <= requested_flush_root,
    accounts_db.flush_accounts_cache(true, Some(requested_flush_root));

    if !is_cache_at_limit {
        // Should flush all slots between 0..=requested_flush_root
        assert_eq!(
            accounts_db.accounts_cache.num_slots(),
            slots.len() - requested_flush_root as usize - 1
        );
    } else {
        // Otherwise, if we are at the cache limit, all roots will be flushed
        assert_eq!(accounts_db.accounts_cache.num_slots(), 0,);
    }

    let expected_max_flushed_root = if !is_cache_at_limit {
        // Should flush all slots between 0..=requested_flush_root
        requested_flush_root
    } else {
        // Otherwise, if we are at the cache limit, all roots will be flushed
        num_slots as Slot - 1
    };

    assert_eq!(
        accounts_db.accounts_cache.fetch_max_flush_root(),
        expected_max_flushed_root,
    );

    for slot in &slots {
        let slot_accounts = accounts_db.scan_account_storage(
            *slot as Slot,
            |loaded_account| {
                assert!(
                    !is_cache_at_limit,
                    "When cache is at limit, all roots should have been flushed to storage"
                );
                // All slots <= requested_flush_root should have been flushed, regardless
                // of ongoing scans
                assert!(*slot > requested_flush_root);
                Some(*loaded_account.pubkey())
            },
            |slot_accounts: &mut HashSet<Pubkey>, stored_account, _data| {
                slot_accounts.insert(*stored_account.pubkey());
                if !is_cache_at_limit {
                    // Only true when the limit hasn't been reached and there are still
                    // slots left in the cache
                    assert!(*slot <= requested_flush_root);
                }
            },
            ScanAccountStorageData::NoData,
        );

        let slot_accounts = match slot_accounts {
            ScanStorageResult::Cached(slot_accounts) => {
                slot_accounts.into_iter().collect::<HashSet<Pubkey>>()
            }
            ScanStorageResult::Stored(slot_accounts) => {
                slot_accounts.into_iter().collect::<HashSet<Pubkey>>()
            }
        };

        let expected_accounts =
            if *slot >= requested_flush_root || *slot >= scan_root.unwrap_or(Slot::MAX) {
                // 1) If slot > `requested_flush_root`, then  either:
                //   a) If `is_cache_at_limit == false`, still in the cache
                //   b) if `is_cache_at_limit == true`, were not cleaned before being flushed to storage.
                //
                // In both cases all the *original* updates at index `slot` were uncleaned and thus
                // should be discoverable by this scan.
                //
                // 2) If slot == `requested_flush_root`, the slot was not cleaned before being flushed to storage,
                // so it also contains all the original updates.
                //
                // 3) If *slot >= scan_root, then we should not clean it either
                keys[*slot as usize..]
                    .iter()
                    .cloned()
                    .collect::<HashSet<Pubkey>>()
            } else {
                // Slots less than `requested_flush_root` and `scan_root` were cleaned in the cache before being flushed
                // to storage, should only contain one account
                std::iter::once(keys[*slot as usize]).collect::<HashSet<Pubkey>>()
            };

        assert_eq!(slot_accounts, expected_accounts);
    }

    if let Some(scan_tracker) = scan_tracker {
        scan_tracker.exit().unwrap();
    }
}

#[test]
fn test_accounts_db_cache_clean_max_root() {
    let requested_flush_root = 5;
    run_test_accounts_db_cache_clean_max_root(10, requested_flush_root, None);
}

#[test]
fn test_accounts_db_cache_clean_max_root_with_scan() {
    let requested_flush_root = 5;
    run_test_accounts_db_cache_clean_max_root(
        10,
        requested_flush_root,
        Some(requested_flush_root - 1),
    );
    run_test_accounts_db_cache_clean_max_root(
        10,
        requested_flush_root,
        Some(requested_flush_root + 1),
    );
}

#[test]
fn test_accounts_db_cache_clean_max_root_with_cache_limit_hit() {
    let requested_flush_root = 5;
    // Test that if there are > max_cache_slots() in the cache after flush, then more roots
    // will be flushed
    run_test_accounts_db_cache_clean_max_root(
        max_cache_slots() + requested_flush_root as usize + 2,
        requested_flush_root,
        None,
    );
}

#[test]
fn test_accounts_db_cache_clean_max_root_with_cache_limit_hit_and_scan() {
    let requested_flush_root = 5;
    // Test that if there are > max_cache_slots() in the cache after flush, then more roots
    // will be flushed
    run_test_accounts_db_cache_clean_max_root(
        max_cache_slots() + requested_flush_root as usize + 2,
        requested_flush_root,
        Some(requested_flush_root - 1),
    );
    run_test_accounts_db_cache_clean_max_root(
        max_cache_slots() + requested_flush_root as usize + 2,
        requested_flush_root,
        Some(requested_flush_root + 1),
    );
}

fn run_flush_rooted_accounts_cache(should_clean: bool) {
    let num_slots = 10;
    let (accounts_db, keys, slots, _) = setup_accounts_db_cache_clean(num_slots, None, None);

    // If no cleaning is specified, then flush everything
    accounts_db.flush_rooted_accounts_cache(None, should_clean);
    for slot in &slots {
        let ScanStorageResult::Stored(slot_accounts) = accounts_db.scan_account_storage(
            *slot as Slot,
            |_| Some(0),
            |slot_account: &mut HashSet<Pubkey>, stored_account, _data| {
                slot_account.insert(*stored_account.pubkey());
            },
            ScanAccountStorageData::NoData,
        ) else {
            panic!("All roots should have been flushed to storage");
        };
        let expected_accounts = if !should_clean || slot == slots.last().unwrap() {
            // The slot was not cleaned before being flushed to storage,
            // so it also contains all the original updates.
            keys[*slot as usize..]
                .iter()
                .cloned()
                .collect::<HashSet<Pubkey>>()
        } else {
            // If clean was specified, only the latest slot should have all the updates.
            // All these other slots have been cleaned before flush
            std::iter::once(keys[*slot as usize]).collect::<HashSet<Pubkey>>()
        };
        assert_eq!(slot_accounts, expected_accounts);
    }
}

#[test]
fn test_flush_rooted_accounts_cache_with_clean() {
    run_flush_rooted_accounts_cache(true);
}

#[test]
fn test_flush_rooted_accounts_cache_without_clean() {
    run_flush_rooted_accounts_cache(false);
}
#[test]
fn test_shrink_unref() {
    let db = AccountsDb::new_single_for_tests();
    let epoch_schedule = EpochSchedule::default();
    let account_key1 = Pubkey::new_unique();
    let account_key2 = Pubkey::new_unique();
    let account1 = AccountSharedData::new(1, 0, AccountSharedData::default().owner());

    // Store into slot 0
    db.store_for_tests((0, [(&account_key1, &account1)].as_slice()));
    db.store_for_tests((0, [(&account_key2, &account1)].as_slice()));
    db.add_root(0);

    // Make account_key1 in slot 0 outdated by updating in rooted slot 1
    db.store_for_tests((1, &[(&account_key1, &account1)][..]));
    db.add_root(1);
    // Flushes all roots
    db.flush_accounts_cache(true, None);

    // Clean to remove outdated entry from slot 0
    db.clean_accounts(Some(1), false, &EpochSchedule::default());

    // Shrink Slot 0
    {
        let mut shrink_candidate_slots = db.shrink_candidate_slots.lock().unwrap();
        shrink_candidate_slots.insert(0);
    }
    db.shrink_candidate_slots(&epoch_schedule);

    // Make slot 0 dead by updating the remaining key
    db.store_for_tests((2, &[(&account_key2, &account1)][..]));
    db.add_root(2);

    // Flushes all roots
    db.flush_accounts_cache(true, None);

    // Should be one store before clean for slot 0
    db.get_and_assert_single_storage(0);
    db.clean_accounts(Some(2), false, &EpochSchedule::default());

    // No stores should exist for slot 0 after clean
    assert_no_storages_at_slot(&db, 0);

    // Ref count for `account_key1` (account removed earlier by shrink)
    // should be 1, since it was only stored in slot 0 and 1, and slot 0
    // is now dead
    assert_eq!(db.accounts_index.ref_count_from_storage(&account_key1), 1);
}

#[test]
fn test_clean_drop_dead_zero_lamport_single_ref_accounts() {
    let accounts_db = AccountsDb::new_single_for_tests();
    let epoch_schedule = EpochSchedule::default();
    let key1 = Pubkey::new_unique();

    let zero_account = AccountSharedData::new(0, 0, AccountSharedData::default().owner());
    let one_account = AccountSharedData::new(1, 0, AccountSharedData::default().owner());

    // slot 0 - stored a 1-lamport account
    let slot = 0;
    accounts_db.store_for_tests((slot, &[(&key1, &one_account)][..]));
    accounts_db.add_root(slot);

    // slot 1 - store a 0 -lamport account
    let slot = 1;
    accounts_db.store_for_tests((slot, &[(&key1, &zero_account)][..]));
    accounts_db.add_root(slot);

    accounts_db.flush_accounts_cache(true, None);

    // run clean
    accounts_db.clean_accounts(Some(1), false, &epoch_schedule);

    // After clean, both slot0 and slot1 should be marked dead and dropped
    // from the store map.
    assert!(accounts_db.storage.get_slot_storage_entry(0).is_none());
    assert!(accounts_db.storage.get_slot_storage_entry(1).is_none());
}

#[test]
fn test_clean_drop_dead_storage_handle_zero_lamport_single_ref_accounts() {
    let db = AccountsDb::new_single_for_tests();
    let account_key1 = Pubkey::new_unique();
    let account_key2 = Pubkey::new_unique();
    let account1 = AccountSharedData::new(1, 0, AccountSharedData::default().owner());
    let account0 = AccountSharedData::new(0, 0, AccountSharedData::default().owner());

    // Store into slot 0
    db.store_for_tests((0, [(&account_key1, &account1)].as_slice()));
    db.add_root_and_flush_write_cache(0);

    // Make account_key1 in slot 0 outdated by updating in rooted slot 1 with a zero lamport account
    // And store one additional live account to make the store still alive after clean.
    db.store_for_tests((1, &[(&account_key1, &account0)][..]));
    db.store_for_tests((1, &[(&account_key2, &account1)][..]));
    db.add_root(1);
    // Flushes all roots
    db.flush_accounts_cache(true, None);

    // Clean should mark slot 0 dead and drop it. During the dropping, it
    // will find that slot 1 has a single ref zero accounts and mark it.
    db.clean_accounts(Some(1), false, &EpochSchedule::default());

    // Assert that after clean, slot 0 is dropped.
    assert!(db.storage.get_slot_storage_entry(0).is_none());

    // And slot 1's single ref zero accounts is marked. Because slot 1 still
    // has one other alive account, it is not completely dead. So it won't
    // be a candidate for "clean" to drop. Instead, it becomes a candidate
    // for next round shrinking.
    assert_eq!(db.accounts_index.ref_count_from_storage(&account_key1), 1);
    assert_eq!(
        db.get_and_assert_single_storage(1)
            .num_zero_lamport_single_ref_accounts(),
        1
    );
    assert!(db.shrink_candidate_slots.lock().unwrap().contains(&1));
}

#[test]
fn test_shrink_unref_handle_zero_lamport_single_ref_accounts() {
    let db = AccountsDb::new_single_for_tests();
    let epoch_schedule = EpochSchedule::default();
    let account_key1 = Pubkey::new_unique();
    let account_key2 = Pubkey::new_unique();
    let account1 = AccountSharedData::new(1, 0, AccountSharedData::default().owner());
    let account0 = AccountSharedData::new(0, 0, AccountSharedData::default().owner());

    // Store into slot 0
    db.store_for_tests((0, [(&account_key1, &account1)].as_slice()));
    db.store_for_tests((0, [(&account_key2, &account1)].as_slice()));
    db.add_root_and_flush_write_cache(0);

    // Make account_key1 in slot 0 outdated by updating in rooted slot 1 with a zero lamport account
    db.store_for_tests((1, &[(&account_key1, &account0)][..]));
    db.add_root(1);
    // Flushes all roots
    db.flush_accounts_cache(true, None);

    // Clean to remove outdated entry from slot 0
    db.clean_accounts(Some(1), false, &EpochSchedule::default());

    // Shrink Slot 0
    {
        let mut shrink_candidate_slots = db.shrink_candidate_slots.lock().unwrap();
        shrink_candidate_slots.insert(0);
    }
    db.shrink_candidate_slots(&epoch_schedule);

    // After shrink slot 0, check that the zero_lamport account on slot 1
    // should be marked since it become singe_ref.
    assert_eq!(db.accounts_index.ref_count_from_storage(&account_key1), 1);
    assert_eq!(
        db.get_and_assert_single_storage(1)
            .num_zero_lamport_single_ref_accounts(),
        1
    );
    // And now, slot 1 should be marked complete dead, which will be added
    // to uncleaned slots, which handle dropping dead storage. And it WON'T
    // be participating shrinking in the next round.
    assert!(db.dirty_stores.contains_key(&1));
    assert!(!db.shrink_candidate_slots.lock().unwrap().contains(&1));

    // Now, make slot 0 dead by updating the remaining key
    db.store_for_tests((2, &[(&account_key2, &account1)][..]));
    db.add_root(2);

    // Flushes all roots
    db.flush_accounts_cache(true, None);

    // Should be one store before clean for slot 0 and slot 1
    db.get_and_assert_single_storage(0);
    db.get_and_assert_single_storage(1);
    db.clean_accounts(Some(2), false, &EpochSchedule::default());

    // No stores should exist for slot 0 after clean
    assert_no_storages_at_slot(&db, 0);
    // No store should exit for slot 1 too as it has only a zero lamport single ref account.
    assert_no_storages_at_slot(&db, 1);
    // Store 2 should have a single account.
    assert_eq!(db.accounts_index.ref_count_from_storage(&account_key2), 1);
    db.get_and_assert_single_storage(2);
}

define_accounts_db_test!(test_partial_clean, |db| {
    let account_key1 = Pubkey::new_unique();
    let account_key2 = Pubkey::new_unique();
    let account1 = AccountSharedData::new(1, 0, AccountSharedData::default().owner());
    let account2 = AccountSharedData::new(2, 0, AccountSharedData::default().owner());
    let account3 = AccountSharedData::new(3, 0, AccountSharedData::default().owner());
    let account4 = AccountSharedData::new(4, 0, AccountSharedData::default().owner());

    // Store accounts into slots 0 and 1
    db.store_for_tests((
        0,
        [(&account_key1, &account1), (&account_key2, &account1)].as_slice(),
    ));
    db.store_for_tests((1, [(&account_key1, &account2)].as_slice()));

    db.print_accounts_stats("pre-clean1");

    // clean accounts - no accounts should be cleaned, since no rooted slots
    //
    // Checking that the uncleaned_pubkeys are not pre-maturely removed
    // such that when the slots are rooted, and can actually be cleaned, then the
    // delta keys are still there.
    db.clean_accounts_for_tests();

    db.print_accounts_stats("post-clean1");

    // Assert that cache entries are still present
    assert!(!db.accounts_cache.slot_cache(0).unwrap().is_empty());
    assert!(!db.accounts_cache.slot_cache(1).unwrap().is_empty());

    // root slot 0
    db.add_root_and_flush_write_cache(0);

    // store into slot 2
    db.store_for_tests((
        2,
        [(&account_key2, &account3), (&account_key1, &account3)].as_slice(),
    ));
    db.clean_accounts_for_tests();
    db.print_accounts_stats("post-clean2");

    // root slots 1
    db.add_root_and_flush_write_cache(1);
    db.clean_accounts_for_tests();

    db.print_accounts_stats("post-clean3");

    db.store_for_tests((3, [(&account_key2, &account4)].as_slice()));
    db.add_root_and_flush_write_cache(3);

    // Check that we can clean where max_root=3 and slot=2 is not rooted
    db.clean_accounts_for_tests();

    assert!(db.uncleaned_pubkeys.is_empty());

    db.print_accounts_stats("post-clean4");

    assert!(db.storage.is_empty_entry(0));
    assert!(!db.storage.is_empty_entry(1));
});

const RACY_SLEEP_MS: u64 = 10;
const RACE_TIME: u64 = 5;

fn start_load_thread(
    with_retry: bool,
    ancestors: Ancestors,
    db: Arc<AccountsDb>,
    exit: Arc<AtomicBool>,
    pubkey: Arc<Pubkey>,
    expected_lamports: impl Fn(&(AccountSharedData, Slot)) -> u64 + Send + 'static,
) -> JoinHandle<()> {
    let load_hint = if with_retry {
        LoadHint::FixedMaxRoot
    } else {
        LoadHint::Unspecified
    };

    std::thread::Builder::new()
        .name("account-do-load".to_string())
        .spawn(move || {
            loop {
                if exit.load(Ordering::Relaxed) {
                    return;
                }
                // Meddle load_limit to cover all branches of implementation.
                // There should absolutely no behavioral difference; the load_limit triggered
                // slow branch should only affect the performance.
                // Ordering::Relaxed is ok because of no data dependencies; the modified field is
                // completely free-standing cfg(test) control-flow knob.
                db.load_limit
                    .store(thread_rng().gen_range(0..10) as u64, Ordering::Relaxed);

                // Load should never be unable to find this key
                let loaded_account = db
                    .do_load(
                        &ancestors,
                        &pubkey,
                        None,
                        load_hint,
                        LOAD_ZERO_LAMPORTS_ANY_TESTS,
                    )
                    .unwrap();
                // slot + 1 == account.lamports because of the account-cache-flush thread
                assert_eq!(
                    loaded_account.0.lamports(),
                    expected_lamports(&loaded_account)
                );
            }
        })
        .unwrap()
}

#[test]
fn test_load_account_and_cache_flush_race() {
    solana_logger::setup();

    let mut db = AccountsDb::new_single_for_tests();
    db.load_delay = RACY_SLEEP_MS;
    let db = Arc::new(db);
    let pubkey = Arc::new(Pubkey::new_unique());
    let exit = Arc::new(AtomicBool::new(false));
    db.store_for_tests((
        0,
        &[(
            pubkey.as_ref(),
            &AccountSharedData::new(1, 0, AccountSharedData::default().owner()),
        )][..],
    ));
    db.add_root(0);
    db.flush_accounts_cache(true, None);

    let t_flush_accounts_cache = {
        let db = db.clone();
        let exit = exit.clone();
        let pubkey = pubkey.clone();
        let mut account = AccountSharedData::new(1, 0, AccountSharedData::default().owner());
        std::thread::Builder::new()
            .name("account-cache-flush".to_string())
            .spawn(move || {
                let mut slot: Slot = 1;
                loop {
                    if exit.load(Ordering::Relaxed) {
                        return;
                    }
                    account.set_lamports(slot + 1);
                    db.store_for_tests((slot, &[(pubkey.as_ref(), &account)][..]));
                    db.add_root(slot);
                    sleep(Duration::from_millis(RACY_SLEEP_MS));
                    db.flush_accounts_cache(true, None);
                    slot += 1;
                }
            })
            .unwrap()
    };

    let t_do_load = start_load_thread(
        false,
        Ancestors::default(),
        db,
        exit.clone(),
        pubkey,
        |(_, slot)| slot + 1,
    );

    sleep(Duration::from_secs(RACE_TIME));
    exit.store(true, Ordering::Relaxed);
    t_flush_accounts_cache.join().unwrap();
    t_do_load.join().map_err(std::panic::resume_unwind).unwrap()
}

fn do_test_load_account_and_shrink_race(with_retry: bool) {
    let mut db = AccountsDb::new_single_for_tests();
    let epoch_schedule = EpochSchedule::default();
    db.load_delay = RACY_SLEEP_MS;
    let db = Arc::new(db);
    let pubkey = Arc::new(Pubkey::new_unique());
    let exit = Arc::new(AtomicBool::new(false));
    let slot = 1;

    // Store an account
    let lamports = 42;
    let mut account = AccountSharedData::new(1, 0, AccountSharedData::default().owner());
    account.set_lamports(lamports);
    db.store_for_tests((slot, [(pubkey.as_ref(), &account)].as_slice()));

    // Set the slot as a root so account loads will see the contents of this slot
    db.add_root(slot);

    let t_shrink_accounts = {
        let db = db.clone();
        let exit = exit.clone();

        std::thread::Builder::new()
            .name("account-shrink".to_string())
            .spawn(move || loop {
                if exit.load(Ordering::Relaxed) {
                    return;
                }
                // Simulate adding shrink candidates from clean_accounts()
                db.shrink_candidate_slots.lock().unwrap().insert(slot);
                db.shrink_candidate_slots(&epoch_schedule);
            })
            .unwrap()
    };

    let t_do_load = start_load_thread(
        with_retry,
        Ancestors::default(),
        db,
        exit.clone(),
        pubkey,
        move |_| lamports,
    );

    sleep(Duration::from_secs(RACE_TIME));
    exit.store(true, Ordering::Relaxed);
    t_shrink_accounts.join().unwrap();
    t_do_load.join().map_err(std::panic::resume_unwind).unwrap()
}

#[test]
fn test_load_account_and_shrink_race_with_retry() {
    do_test_load_account_and_shrink_race(true);
}

#[test]
fn test_load_account_and_shrink_race_without_retry() {
    do_test_load_account_and_shrink_race(false);
}

#[test]
fn test_cache_flush_delayed_remove_unrooted_race() {
    let mut db = AccountsDb::new_single_for_tests();
    db.load_delay = RACY_SLEEP_MS;
    let db = Arc::new(db);
    let slot = 10;
    let bank_id = 10;

    let lamports = 42;
    let mut account = AccountSharedData::new(1, 0, AccountSharedData::default().owner());
    account.set_lamports(lamports);

    // Start up a thread to flush the accounts cache
    let (flush_trial_start_sender, flush_trial_start_receiver) = crossbeam_channel::unbounded();
    let (flush_done_sender, flush_done_receiver) = crossbeam_channel::unbounded();
    let t_flush_cache = {
        let db = db.clone();
        std::thread::Builder::new()
            .name("account-cache-flush".to_string())
            .spawn(move || loop {
                // Wait for the signal to start a trial
                if flush_trial_start_receiver.recv().is_err() {
                    return;
                }
                db.flush_slot_cache(10);
                flush_done_sender.send(()).unwrap();
            })
            .unwrap()
    };

    // Start up a thread remove the slot
    let (remove_trial_start_sender, remove_trial_start_receiver) = crossbeam_channel::unbounded();
    let (remove_done_sender, remove_done_receiver) = crossbeam_channel::unbounded();
    let t_remove = {
        let db = db.clone();
        std::thread::Builder::new()
            .name("account-remove".to_string())
            .spawn(move || loop {
                // Wait for the signal to start a trial
                if remove_trial_start_receiver.recv().is_err() {
                    return;
                }
                db.remove_unrooted_slots(&[(slot, bank_id)]);
                remove_done_sender.send(()).unwrap();
            })
            .unwrap()
    };

    let num_trials = 10;
    for _ in 0..num_trials {
        let pubkey = Pubkey::new_unique();
        db.store_for_tests((slot, &[(&pubkey, &account)][..]));
        // Wait for both threads to finish
        flush_trial_start_sender.send(()).unwrap();
        remove_trial_start_sender.send(()).unwrap();
        let _ = flush_done_receiver.recv();
        let _ = remove_done_receiver.recv();
    }

    drop(flush_trial_start_sender);
    drop(remove_trial_start_sender);
    t_flush_cache.join().unwrap();
    t_remove.join().unwrap();
}

#[test]
fn test_cache_flush_remove_unrooted_race_multiple_slots() {
    let db = AccountsDb::new_single_for_tests();
    let db = Arc::new(db);
    let num_cached_slots = 100;

    let num_trials = 100;
    let (new_trial_start_sender, new_trial_start_receiver) = crossbeam_channel::unbounded();
    let (flush_done_sender, flush_done_receiver) = crossbeam_channel::unbounded();
    // Start up a thread to flush the accounts cache
    let t_flush_cache = {
        let db = db.clone();

        std::thread::Builder::new()
            .name("account-cache-flush".to_string())
            .spawn(move || loop {
                // Wait for the signal to start a trial
                if new_trial_start_receiver.recv().is_err() {
                    return;
                }
                for slot in 0..num_cached_slots {
                    db.flush_slot_cache(slot);
                }
                flush_done_sender.send(()).unwrap();
            })
            .unwrap()
    };

    let exit = Arc::new(AtomicBool::new(false));

    let t_spurious_signal = {
        let db = db.clone();
        let exit = exit.clone();
        std::thread::Builder::new()
            .name("account-cache-flush".to_string())
            .spawn(move || loop {
                if exit.load(Ordering::Relaxed) {
                    return;
                }
                // Simulate spurious wake-up that can happen, but is too rare to
                // otherwise depend on in tests.
                db.remove_unrooted_slots_synchronization.signal.notify_all();
            })
            .unwrap()
    };

    // Run multiple trials. Has the added benefit of rewriting the same slots after we've
    // dumped them in previous trials.
    for _ in 0..num_trials {
        // Store an account
        let lamports = 42;
        let mut account = AccountSharedData::new(1, 0, AccountSharedData::default().owner());
        account.set_lamports(lamports);

        // Pick random 50% of the slots to pass to `remove_unrooted_slots()`
        let mut all_slots: Vec<(Slot, BankId)> = (0..num_cached_slots)
            .map(|slot| {
                let bank_id = slot + 1;
                (slot, bank_id)
            })
            .collect();
        all_slots.shuffle(&mut rand::thread_rng());
        let slots_to_dump = &all_slots[0..num_cached_slots as usize / 2];
        let slots_to_keep = &all_slots[num_cached_slots as usize / 2..];

        // Set up a one account per slot across many different slots, track which
        // pubkey was stored in each slot.
        let slot_to_pubkey_map: HashMap<Slot, Pubkey> = (0..num_cached_slots)
            .map(|slot| {
                let pubkey = Pubkey::new_unique();
                db.store_for_tests((slot, &[(&pubkey, &account)][..]));
                (slot, pubkey)
            })
            .collect();

        // Signal the flushing shred to start flushing
        new_trial_start_sender.send(()).unwrap();

        // Here we want to test both:
        // 1) Flush thread starts flushing a slot before we try dumping it.
        // 2) Flushing thread trying to flush while/after we're trying to dump the slot,
        // in which case flush should ignore/move past the slot to be dumped
        //
        // Hence, we split into chunks to get the dumping of each chunk to race with the
        // flushes. If we were to dump the entire chunk at once, then this reduces the possibility
        // of the flush occurring first since the dumping logic reserves all the slots it's about
        // to dump immediately.

        for chunks in slots_to_dump.chunks(slots_to_dump.len() / 2) {
            db.remove_unrooted_slots(chunks);
        }

        // Check that all the slots in `slots_to_dump` were completely removed from the
        // cache, storage, and index

        for (slot, _) in slots_to_dump {
            assert_no_storages_at_slot(&db, *slot);
            assert!(db.accounts_cache.slot_cache(*slot).is_none());
            let account_in_slot = slot_to_pubkey_map[slot];
            assert!(!db.accounts_index.contains(&account_in_slot));
        }

        // Wait for flush to finish before starting next trial

        flush_done_receiver.recv().unwrap();

        for (slot, bank_id) in slots_to_keep {
            let account_in_slot = slot_to_pubkey_map[slot];
            assert!(db
                .load(
                    &Ancestors::from(vec![(*slot, 0)]),
                    &account_in_slot,
                    LoadHint::FixedMaxRoot
                )
                .is_some());
            // Clear for next iteration so that `assert!(self.storage.get_slot_storage_entry(purged_slot).is_none());`
            // in `purge_slot_pubkeys()` doesn't trigger
            db.remove_unrooted_slots(&[(*slot, *bank_id)]);
        }
    }

    exit.store(true, Ordering::Relaxed);
    drop(new_trial_start_sender);
    t_flush_cache.join().unwrap();

    t_spurious_signal.join().unwrap();
}

#[test]
fn test_collect_uncleaned_slots_up_to_slot() {
    solana_logger::setup();
    let db = AccountsDb::new_single_for_tests();

    let slot1 = 11;
    let slot2 = 222;
    let slot3 = 3333;

    let pubkey1 = Pubkey::new_unique();
    let pubkey2 = Pubkey::new_unique();
    let pubkey3 = Pubkey::new_unique();

    db.uncleaned_pubkeys.insert(slot1, vec![pubkey1]);
    db.uncleaned_pubkeys.insert(slot2, vec![pubkey2]);
    db.uncleaned_pubkeys.insert(slot3, vec![pubkey3]);

    let mut uncleaned_slots1 = db.collect_uncleaned_slots_up_to_slot(slot1);
    let mut uncleaned_slots2 = db.collect_uncleaned_slots_up_to_slot(slot2);
    let mut uncleaned_slots3 = db.collect_uncleaned_slots_up_to_slot(slot3);

    uncleaned_slots1.sort_unstable();
    uncleaned_slots2.sort_unstable();
    uncleaned_slots3.sort_unstable();

    assert_eq!(uncleaned_slots1, [slot1]);
    assert_eq!(uncleaned_slots2, [slot1, slot2]);
    assert_eq!(uncleaned_slots3, [slot1, slot2, slot3]);
}

#[test]
fn test_remove_uncleaned_slots_and_collect_pubkeys_up_to_slot() {
    solana_logger::setup();
    let db = AccountsDb::new_single_for_tests();

    let slot1 = 11;
    let slot2 = 222;
    let slot3 = 3333;

    let pubkey1 = Pubkey::new_unique();
    let pubkey2 = Pubkey::new_unique();
    let pubkey3 = Pubkey::new_unique();

    let account1 = AccountSharedData::new(0, 0, &pubkey1);
    let account2 = AccountSharedData::new(0, 0, &pubkey2);
    let account3 = AccountSharedData::new(0, 0, &pubkey3);

    db.store_for_tests((slot1, [(&pubkey1, &account1)].as_slice()));
    db.store_for_tests((slot2, [(&pubkey2, &account2)].as_slice()));
    db.store_for_tests((slot3, [(&pubkey3, &account3)].as_slice()));

    // slot 1 is _not_ a root on purpose
    db.add_root(slot2);
    db.add_root(slot3);

    db.uncleaned_pubkeys.insert(slot1, vec![pubkey1]);
    db.uncleaned_pubkeys.insert(slot2, vec![pubkey2]);
    db.uncleaned_pubkeys.insert(slot3, vec![pubkey3]);

    let num_bins = db.accounts_index.bins();
    let candidates: Box<_> =
        std::iter::repeat_with(|| RwLock::new(HashMap::<Pubkey, CleaningInfo>::new()))
            .take(num_bins)
            .collect();
    db.remove_uncleaned_slots_up_to_slot_and_move_pubkeys(slot3, &candidates);

    let candidates_contain = |pubkey: &Pubkey| {
        candidates
            .iter()
            .any(|bin| bin.read().unwrap().contains(pubkey))
    };
    assert!(candidates_contain(&pubkey1));
    assert!(candidates_contain(&pubkey2));
    assert!(candidates_contain(&pubkey3));
}

#[test]
fn test_shrink_productive() {
    solana_logger::setup();
    let path = Path::new("");
    let file_size = 100;
    let slot = 11;

    let store = Arc::new(AccountStorageEntry::new(
        path,
        slot,
        slot as AccountsFileId,
        file_size,
        AccountsFileProvider::AppendVec,
    ));
    store.add_account(file_size as usize);
    assert!(!AccountsDb::is_shrinking_productive(&store));

    let store = Arc::new(AccountStorageEntry::new(
        path,
        slot,
        slot as AccountsFileId,
        file_size,
        AccountsFileProvider::AppendVec,
    ));
    store.add_account(file_size as usize / 2);
    store.add_account(file_size as usize / 4);
    store.remove_accounts(file_size as usize / 4, 1);
    assert!(AccountsDb::is_shrinking_productive(&store));

    store.add_account(file_size as usize / 2);
    assert!(!AccountsDb::is_shrinking_productive(&store));
}

#[test]
fn test_is_candidate_for_shrink() {
    solana_logger::setup();

    let mut accounts = AccountsDb::new_single_for_tests();
    let common_store_path = Path::new("");
    let store_file_size = 100_000;
    let entry = Arc::new(AccountStorageEntry::new(
        common_store_path,
        0,
        1,
        store_file_size,
        AccountsFileProvider::AppendVec,
    ));
    match accounts.shrink_ratio {
        AccountShrinkThreshold::TotalSpace { shrink_ratio } => {
            assert_eq!(
                (DEFAULT_ACCOUNTS_SHRINK_RATIO * 100.) as u64,
                (shrink_ratio * 100.) as u64
            )
        }
        AccountShrinkThreshold::IndividualStore { shrink_ratio: _ } => {
            panic!("Expect the default to be TotalSpace")
        }
    }

    entry
        .alive_bytes
        .store(store_file_size as usize - 1, Ordering::Release);
    assert!(accounts.is_candidate_for_shrink(&entry));
    entry
        .alive_bytes
        .store(store_file_size as usize, Ordering::Release);
    assert!(!accounts.is_candidate_for_shrink(&entry));

    let shrink_ratio = 0.3;
    let file_size_shrink_limit = (store_file_size as f64 * shrink_ratio) as usize;
    entry
        .alive_bytes
        .store(file_size_shrink_limit + 1, Ordering::Release);
    accounts.shrink_ratio = AccountShrinkThreshold::TotalSpace { shrink_ratio };
    assert!(accounts.is_candidate_for_shrink(&entry));
    accounts.shrink_ratio = AccountShrinkThreshold::IndividualStore { shrink_ratio };
    assert!(!accounts.is_candidate_for_shrink(&entry));
}

define_accounts_db_test!(test_calculate_storage_count_and_alive_bytes, |accounts| {
    accounts.accounts_index.set_startup(Startup::Startup);
    let shared_key = solana_pubkey::new_rand();
    let account = AccountSharedData::new(1, 1, AccountSharedData::default().owner());
    let slot0 = 0;

    accounts.accounts_index.set_startup(Startup::Startup);

    let storage = accounts.create_and_insert_store(slot0, 4_000, "flush_slot_cache");
    storage
        .accounts
        .write_accounts(&(slot0, &[(&shared_key, &account)][..]), 0);

    let storage = accounts.storage.get_slot_storage_entry(slot0).unwrap();
    let storage_info = StorageSizeAndCountMap::default();
    let mut reader = append_vec::new_scan_accounts_reader();
    accounts.generate_index_for_slot(&mut reader, &storage, slot0, 0, &storage_info);
    assert_eq!(storage_info.len(), 1);
    for entry in storage_info.iter() {
        let expected_stored_size =
            if accounts.accounts_file_provider == AccountsFileProvider::HotStorage {
                33
            } else {
                144
            };
        assert_eq!(
            (entry.key(), entry.value().count, entry.value().stored_size),
            (&0, 1, expected_stored_size)
        );
    }
    accounts.accounts_index.set_startup(Startup::Normal);
});

define_accounts_db_test!(
    test_calculate_storage_count_and_alive_bytes_0_accounts,
    |accounts| {
        // empty store
        let storage = accounts.create_and_insert_store(0, 1, "test");
        let storage_info = StorageSizeAndCountMap::default();
        let mut reader = append_vec::new_scan_accounts_reader();
        accounts.generate_index_for_slot(&mut reader, &storage, 0, 0, &storage_info);
        assert!(storage_info.is_empty());
    }
);

define_accounts_db_test!(
    test_calculate_storage_count_and_alive_bytes_2_accounts,
    |accounts| {
        let keys = [
            solana_pubkey::Pubkey::from([0; 32]),
            solana_pubkey::Pubkey::from([255; 32]),
        ];
        accounts.accounts_index.set_startup(Startup::Startup);

        // make sure accounts are in 2 different bins
        assert!(
            (accounts.accounts_index.bins() == 1)
                ^ (accounts
                    .accounts_index
                    .bin_calculator
                    .bin_from_pubkey(&keys[0])
                    != accounts
                        .accounts_index
                        .bin_calculator
                        .bin_from_pubkey(&keys[1]))
        );
        let account = AccountSharedData::new(1, 1, AccountSharedData::default().owner());
        let account_big = AccountSharedData::new(1, 1000, AccountSharedData::default().owner());
        let slot0 = 0;
        let storage = accounts.create_and_insert_store(slot0, 4_000, "flush_slot_cache");
        storage.accounts.write_accounts(
            &(slot0, &[(&keys[0], &account), (&keys[1], &account_big)][..]),
            0,
        );

        let storage_info = StorageSizeAndCountMap::default();
        let mut reader = append_vec::new_scan_accounts_reader();
        accounts.generate_index_for_slot(&mut reader, &storage, 0, 0, &storage_info);
        assert_eq!(storage_info.len(), 1);
        for entry in storage_info.iter() {
            let expected_stored_size =
                if accounts.accounts_file_provider == AccountsFileProvider::HotStorage {
                    1065
                } else {
                    1280
                };
            assert_eq!(
                (entry.key(), entry.value().count, entry.value().stored_size),
                (&0, 2, expected_stored_size)
            );
        }
        accounts.accounts_index.set_startup(Startup::Normal);
    }
);

define_accounts_db_test!(test_set_storage_count_and_alive_bytes, |accounts| {
    // make sure we have storage 0
    let shared_key = solana_pubkey::new_rand();
    let account = AccountSharedData::new(1, 1, AccountSharedData::default().owner());
    let slot0 = 0;
    accounts.store_for_tests((slot0, [(&shared_key, &account)].as_slice()));
    accounts.add_root_and_flush_write_cache(slot0);

    // fake out the store count to avoid the assert
    for (_, store) in accounts.storage.iter() {
        store.alive_bytes.store(0, Ordering::Release);
        let mut count_and_status = store.count_and_status.lock_write();
        count_and_status.0 = 0;
    }

    // count needs to be <= approx stored count in store.
    // approx stored count is 1 in store since we added a single account.
    let count = 1;

    // populate based on made up hash data
    let dashmap = DashMap::default();
    dashmap.insert(
        0,
        StorageSizeAndCount {
            stored_size: 2,
            count,
        },
    );

    for (_, store) in accounts.storage.iter() {
        assert_eq!(store.count_and_status.read().0, 0);
        assert_eq!(store.alive_bytes(), 0);
    }
    accounts.set_storage_count_and_alive_bytes(dashmap, &mut GenerateIndexTimings::default());
    assert_eq!(accounts.storage.len(), 1);
    for (_, store) in accounts.storage.iter() {
        assert_eq!(store.id(), 0);
        assert_eq!(store.count_and_status.read().0, count);
        assert_eq!(store.alive_bytes(), 2);
    }
});

define_accounts_db_test!(test_purge_alive_unrooted_slots_after_clean, |accounts| {
    // Key shared between rooted and nonrooted slot
    let shared_key = solana_pubkey::new_rand();
    // Key to keep the storage entry for the unrooted slot alive
    let unrooted_key = solana_pubkey::new_rand();
    let slot0 = 0;
    let slot1 = 1;

    // Store accounts with greater than 0 lamports
    let account = AccountSharedData::new(1, 1, AccountSharedData::default().owner());
    accounts.store_for_tests((slot0, [(&shared_key, &account)].as_slice()));
    accounts.store_for_tests((slot0, [(&unrooted_key, &account)].as_slice()));

    // Simulate adding dirty pubkeys on bank freeze. Note this is
    // not a rooted slot

    // On the next *rooted* slot, update the `shared_key` account to zero lamports
    let zero_lamport_account = AccountSharedData::new(0, 0, AccountSharedData::default().owner());
    accounts.store_for_tests((slot1, [(&shared_key, &zero_lamport_account)].as_slice()));

    // Simulate adding dirty pubkeys on bank freeze, set root
    accounts.add_root_and_flush_write_cache(slot1);

    // The later rooted zero-lamport update to `shared_key` cannot be cleaned
    // because it is kept alive by the unrooted slot.
    accounts.clean_accounts_for_tests();
    assert!(accounts.accounts_index.contains(&shared_key));

    // Simulate purge_slot() all from AccountsBackgroundService
    accounts.purge_slot(slot0, 0, true);

    // Now clean should clean up the remaining key
    accounts.clean_accounts_for_tests();
    assert!(!accounts.accounts_index.contains(&shared_key));
    assert_no_storages_at_slot(&accounts, slot0);
});

/// asserts that not only are there 0 append vecs, but there is not even an entry in the storage map for 'slot'
fn assert_no_storages_at_slot(db: &AccountsDb, slot: Slot) {
    assert!(db.storage.get_slot_storage_entry(slot).is_none());
}

// Test to make sure `clean_accounts()` works properly with `latest_full_snapshot_slot`
//
// Basically:
//
// - slot 1: set Account1's balance to non-zero
// - slot 2: set Account1's balance to a different non-zero amount
// - slot 3: set Account1's balance to zero
// - call `clean_accounts()` with `max_clean_root` set to 2
//     - ensure Account1 has *not* been purged
//     - ensure the store from slot 1 is cleaned up
// - call `clean_accounts()` with `latest_full_snapshot_slot` set to 2
//     - ensure Account1 has *not* been purged
// - call `clean_accounts()` with `latest_full_snapshot_slot` set to 3
//     - ensure Account1 *has* been purged
define_accounts_db_test!(
    test_clean_accounts_with_latest_full_snapshot_slot,
    |accounts_db| {
        let pubkey = solana_pubkey::new_rand();
        let owner = solana_pubkey::new_rand();
        let space = 0;

        let slot1: Slot = 1;
        let account = AccountSharedData::new(111, space, &owner);
        accounts_db.store_for_tests((slot1, &[(&pubkey, &account)][..]));
        accounts_db.add_root_and_flush_write_cache(slot1);

        let slot2: Slot = 2;
        let account = AccountSharedData::new(222, space, &owner);
        accounts_db.store_for_tests((slot2, &[(&pubkey, &account)][..]));
        accounts_db.add_root_and_flush_write_cache(slot2);

        let slot3: Slot = 3;
        let account = AccountSharedData::new(0, space, &owner);
        accounts_db.store_for_tests((slot3, &[(&pubkey, &account)][..]));
        accounts_db.add_root_and_flush_write_cache(slot3);

        assert_eq!(accounts_db.ref_count_for_pubkey(&pubkey), 3);

        accounts_db.set_latest_full_snapshot_slot(slot2);
        accounts_db.clean_accounts(Some(slot2), false, &EpochSchedule::default());
        assert_eq!(accounts_db.ref_count_for_pubkey(&pubkey), 2);

        accounts_db.set_latest_full_snapshot_slot(slot2);
        accounts_db.clean_accounts(None, false, &EpochSchedule::default());
        assert_eq!(accounts_db.ref_count_for_pubkey(&pubkey), 1);

        accounts_db.set_latest_full_snapshot_slot(slot3);
        accounts_db.clean_accounts(None, false, &EpochSchedule::default());
        assert_eq!(accounts_db.ref_count_for_pubkey(&pubkey), 0);
    }
);

#[test]
fn test_filter_zero_lamport_clean_for_incremental_snapshots() {
    solana_logger::setup();
    let slot = 10;

    struct TestParameters {
        latest_full_snapshot_slot: Option<Slot>,
        max_clean_root: Option<Slot>,
        should_contain: bool,
    }

    let do_test = |test_params: TestParameters| {
        let account_info = AccountInfo::new(StorageLocation::AppendVec(42, 128), true);
        let pubkey = solana_pubkey::new_rand();
        let mut key_set = HashSet::default();
        key_set.insert(pubkey);
        let store_count = 0;
        let mut store_counts = HashMap::default();
        store_counts.insert(slot, (store_count, key_set));
        let mut candidates = [HashMap::new()];
        candidates[0].insert(
            pubkey,
            CleaningInfo {
                slot_list: vec![(slot, account_info)],
                ref_count: 1,
                ..Default::default()
            },
        );
        let accounts_db = AccountsDb::new_single_for_tests();
        if let Some(latest_full_snapshot_slot) = test_params.latest_full_snapshot_slot {
            accounts_db.set_latest_full_snapshot_slot(latest_full_snapshot_slot);
        }
        accounts_db.filter_zero_lamport_clean_for_incremental_snapshots(
            test_params.max_clean_root,
            &store_counts,
            &mut candidates,
        );

        assert_eq!(
            candidates[0].contains_key(&pubkey),
            test_params.should_contain
        );
    };

    // Scenario 1: last full snapshot is NONE
    // In this scenario incremental snapshots are OFF, so always purge
    {
        let latest_full_snapshot_slot = None;

        do_test(TestParameters {
            latest_full_snapshot_slot,
            max_clean_root: Some(slot),
            should_contain: true,
        });

        do_test(TestParameters {
            latest_full_snapshot_slot,
            max_clean_root: None,
            should_contain: true,
        });
    }

    // Scenario 2: last full snapshot is GREATER THAN zero lamport account slot
    // In this scenario always purge, and just test the various permutations of
    // `should_filter_for_incremental_snapshots` based on `max_clean_root`.
    {
        let latest_full_snapshot_slot = Some(slot + 1);

        do_test(TestParameters {
            latest_full_snapshot_slot,
            max_clean_root: latest_full_snapshot_slot,
            should_contain: true,
        });

        do_test(TestParameters {
            latest_full_snapshot_slot,
            max_clean_root: latest_full_snapshot_slot.map(|s| s + 1),
            should_contain: true,
        });

        do_test(TestParameters {
            latest_full_snapshot_slot,
            max_clean_root: None,
            should_contain: true,
        });
    }

    // Scenario 3: last full snapshot is EQUAL TO zero lamport account slot
    // In this scenario always purge, as it's the same as Scenario 2.
    {
        let latest_full_snapshot_slot = Some(slot);

        do_test(TestParameters {
            latest_full_snapshot_slot,
            max_clean_root: latest_full_snapshot_slot,
            should_contain: true,
        });

        do_test(TestParameters {
            latest_full_snapshot_slot,
            max_clean_root: latest_full_snapshot_slot.map(|s| s + 1),
            should_contain: true,
        });

        do_test(TestParameters {
            latest_full_snapshot_slot,
            max_clean_root: None,
            should_contain: true,
        });
    }

    // Scenario 4: last full snapshot is LESS THAN zero lamport account slot
    // In this scenario do *not* purge, except when `should_filter_for_incremental_snapshots`
    // is false
    {
        let latest_full_snapshot_slot = Some(slot - 1);

        do_test(TestParameters {
            latest_full_snapshot_slot,
            max_clean_root: latest_full_snapshot_slot,
            should_contain: true,
        });

        do_test(TestParameters {
            latest_full_snapshot_slot,
            max_clean_root: latest_full_snapshot_slot.map(|s| s + 1),
            should_contain: false,
        });

        do_test(TestParameters {
            latest_full_snapshot_slot,
            max_clean_root: None,
            should_contain: false,
        });
    }
}

impl AccountsDb {
    /// helper function to test unref_accounts  clean_dead_slots_from_accounts_index
    fn test_unref(
        &self,
        call_clean: bool,
        purged_slot_pubkeys: HashSet<(Slot, Pubkey)>,
        purged_stored_account_slots: &mut AccountSlots,
        pubkeys_removed_from_accounts_index: &PubkeysRemovedFromAccountsIndex,
    ) {
        self.unref_accounts(
            purged_slot_pubkeys,
            purged_stored_account_slots,
            pubkeys_removed_from_accounts_index,
        );

        if call_clean {
            let empty_vec = Vec::default();
            self.clean_dead_slots_from_accounts_index(empty_vec.iter());
        }
    }
}

#[test]
/// test 'unref' parameter 'pubkeys_removed_from_accounts_index'
fn test_unref_pubkeys_removed_from_accounts_index() {
    let slot1 = 1;
    let pk1 = Pubkey::from([1; 32]);
    for already_removed in [false, true] {
        let mut pubkeys_removed_from_accounts_index = PubkeysRemovedFromAccountsIndex::default();
        if already_removed {
            pubkeys_removed_from_accounts_index.insert(pk1);
        }
        // pk1 in slot1, purge it
        let db = AccountsDb::new_single_for_tests();
        let mut purged_slot_pubkeys = HashSet::default();
        purged_slot_pubkeys.insert((slot1, pk1));
        let mut reclaims = SlotList::default();
        db.accounts_index.upsert(
            slot1,
            slot1,
            &pk1,
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            AccountInfo::default(),
            &mut reclaims,
            UpsertReclaim::IgnoreReclaims,
        );

        let mut purged_stored_account_slots = AccountSlots::default();
        db.test_unref(
            false,
            purged_slot_pubkeys,
            &mut purged_stored_account_slots,
            &pubkeys_removed_from_accounts_index,
        );
        assert_eq!(
            vec![(pk1, vec![slot1].into_iter().collect::<IntSet<_>>())],
            purged_stored_account_slots.into_iter().collect::<Vec<_>>()
        );
        let expected = u64::from(already_removed);
        assert_eq!(db.accounts_index.ref_count_from_storage(&pk1), expected);
    }
}

#[test]
fn test_unref_accounts() {
    let pubkeys_removed_from_accounts_index = PubkeysRemovedFromAccountsIndex::default();
    for call_clean in [true, false] {
        {
            let db = AccountsDb::new_single_for_tests();
            let mut purged_stored_account_slots = AccountSlots::default();

            db.test_unref(
                call_clean,
                HashSet::default(),
                &mut purged_stored_account_slots,
                &pubkeys_removed_from_accounts_index,
            );
            assert!(purged_stored_account_slots.is_empty());
        }

        let slot1 = 1;
        let slot2 = 2;
        let pk1 = Pubkey::from([1; 32]);
        let pk2 = Pubkey::from([2; 32]);
        {
            // pk1 in slot1, purge it
            let db = AccountsDb::new_single_for_tests();
            let mut purged_slot_pubkeys = HashSet::default();
            purged_slot_pubkeys.insert((slot1, pk1));
            let mut reclaims = SlotList::default();
            db.accounts_index.upsert(
                slot1,
                slot1,
                &pk1,
                &AccountSharedData::default(),
                &AccountSecondaryIndexes::default(),
                AccountInfo::default(),
                &mut reclaims,
                UpsertReclaim::IgnoreReclaims,
            );

            let mut purged_stored_account_slots = AccountSlots::default();
            db.test_unref(
                call_clean,
                purged_slot_pubkeys,
                &mut purged_stored_account_slots,
                &pubkeys_removed_from_accounts_index,
            );
            assert_eq!(
                vec![(pk1, vec![slot1].into_iter().collect::<IntSet<_>>())],
                purged_stored_account_slots.into_iter().collect::<Vec<_>>()
            );
            assert_eq!(db.accounts_index.ref_count_from_storage(&pk1), 0);
        }
        {
            let db = AccountsDb::new_single_for_tests();
            let mut purged_stored_account_slots = AccountSlots::default();
            let mut purged_slot_pubkeys = HashSet::default();
            let mut reclaims = SlotList::default();
            // pk1 and pk2 both in slot1 and slot2, so each has refcount of 2
            for slot in [slot1, slot2] {
                for pk in [pk1, pk2] {
                    db.accounts_index.upsert(
                        slot,
                        slot,
                        &pk,
                        &AccountSharedData::default(),
                        &AccountSecondaryIndexes::default(),
                        AccountInfo::default(),
                        &mut reclaims,
                        UpsertReclaim::IgnoreReclaims,
                    );
                }
            }
            // purge pk1 from both 1 and 2 and pk2 from slot 1
            let purges = vec![(slot1, pk1), (slot1, pk2), (slot2, pk1)];
            purges.into_iter().for_each(|(slot, pk)| {
                purged_slot_pubkeys.insert((slot, pk));
            });
            db.test_unref(
                call_clean,
                purged_slot_pubkeys,
                &mut purged_stored_account_slots,
                &pubkeys_removed_from_accounts_index,
            );
            for (pk, slots) in [(pk1, vec![slot1, slot2]), (pk2, vec![slot1])] {
                let result = purged_stored_account_slots.remove(&pk).unwrap();
                assert_eq!(result, slots.into_iter().collect::<IntSet<_>>());
            }
            assert!(purged_stored_account_slots.is_empty());
            assert_eq!(db.accounts_index.ref_count_from_storage(&pk1), 0);
            assert_eq!(db.accounts_index.ref_count_from_storage(&pk2), 1);
        }
    }
}

define_accounts_db_test!(test_many_unrefs, |db| {
    let mut purged_stored_account_slots = AccountSlots::default();
    let mut reclaims = SlotList::default();
    let pk1 = Pubkey::from([1; 32]);
    // make sure we have > 1 batch. Bigger numbers cost more in test time here.
    let n = (UNREF_ACCOUNTS_BATCH_SIZE + 1) as Slot;
    // put the pubkey into the acct idx in 'n' slots
    let purged_slot_pubkeys = (0..n)
        .map(|slot| {
            db.accounts_index.upsert(
                slot,
                slot,
                &pk1,
                &AccountSharedData::default(),
                &AccountSecondaryIndexes::default(),
                AccountInfo::default(),
                &mut reclaims,
                UpsertReclaim::IgnoreReclaims,
            );
            (slot, pk1)
        })
        .collect::<HashSet<_>>();

    assert_eq!(db.accounts_index.ref_count_from_storage(&pk1), n);
    // unref all 'n' slots
    db.unref_accounts(
        purged_slot_pubkeys,
        &mut purged_stored_account_slots,
        &HashSet::default(),
    );
    assert_eq!(db.accounts_index.ref_count_from_storage(&pk1), 0);
});

define_accounts_db_test!(test_mark_dirty_dead_stores_empty, |db| {
    let slot = 0;
    for add_dirty_stores in [false, true] {
        let dead_storages = db.mark_dirty_dead_stores(slot, add_dirty_stores, None, false);
        assert!(dead_storages.is_empty());
        assert!(db.dirty_stores.is_empty());
    }
});

#[test]
fn test_mark_dirty_dead_stores_no_shrink_in_progress() {
    // None for shrink_in_progress, 1 existing store at the slot
    // There should be no more append vecs at that slot after the call to mark_dirty_dead_stores.
    // This tests the case where this slot was combined into an ancient append vec from an older slot and
    // there is no longer an append vec at this slot.
    for add_dirty_stores in [false, true] {
        let slot = 0;
        let db = AccountsDb::new_single_for_tests();
        let size = 1;
        let existing_store = db.create_and_insert_store(slot, size, "test");
        let old_id = existing_store.id();
        let dead_storages = db.mark_dirty_dead_stores(slot, add_dirty_stores, None, false);
        assert!(db.storage.get_slot_storage_entry(slot).is_none());
        assert_eq!(dead_storages.len(), 1);
        assert_eq!(dead_storages.first().unwrap().id(), old_id);
        if add_dirty_stores {
            assert_eq!(1, db.dirty_stores.len());
            let dirty_store = db.dirty_stores.get(&slot).unwrap();
            assert_eq!(dirty_store.id(), old_id);
        } else {
            assert!(db.dirty_stores.is_empty());
        }
        assert!(db.storage.is_empty_entry(slot));
    }
}

#[test]
fn test_mark_dirty_dead_stores() {
    let slot = 0;

    // use shrink_in_progress to cause us to drop the initial store
    for add_dirty_stores in [false, true] {
        let db = AccountsDb::new_single_for_tests();
        let size = 1;
        let old_store = db.create_and_insert_store(slot, size, "test");
        let old_id = old_store.id();
        let shrink_in_progress = db.get_store_for_shrink(slot, 100);
        let dead_storages =
            db.mark_dirty_dead_stores(slot, add_dirty_stores, Some(shrink_in_progress), false);
        assert!(db.storage.get_slot_storage_entry(slot).is_some());
        assert_eq!(dead_storages.len(), 1);
        assert_eq!(dead_storages.first().unwrap().id(), old_id);
        if add_dirty_stores {
            assert_eq!(1, db.dirty_stores.len());
            let dirty_store = db.dirty_stores.get(&slot).unwrap();
            assert_eq!(dirty_store.id(), old_id);
        } else {
            assert!(db.dirty_stores.is_empty());
        }
        assert!(db.storage.get_slot_storage_entry(slot).is_some());
    }
}

define_accounts_db_test!(test_add_uncleaned_pubkeys_after_shrink, |db| {
    let slot = 0;
    let pubkey = Pubkey::from([1; 32]);
    db.add_uncleaned_pubkeys_after_shrink(slot, vec![pubkey].into_iter());
    assert_eq!(&*db.uncleaned_pubkeys.get(&slot).unwrap(), &vec![pubkey]);
});

#[test]
fn test_sweep_get_oldest_non_ancient_slot_max() {
    let epoch_schedule = EpochSchedule::default();
    // way into future
    for ancient_append_vec_offset in [
        epoch_schedule.slots_per_epoch,
        epoch_schedule.slots_per_epoch + 1,
        epoch_schedule.slots_per_epoch * 2,
    ] {
        let db = AccountsDb::new_with_config(
            Vec::new(),
            Some(AccountsDbConfig {
                ancient_append_vec_offset: Some(ancient_append_vec_offset as i64),
                ..ACCOUNTS_DB_CONFIG_FOR_TESTING
            }),
            None,
            Arc::default(),
        );
        // before any roots are added, we expect the oldest non-ancient slot to be 0
        assert_eq!(0, db.get_oldest_non_ancient_slot(&epoch_schedule));
        for max_root_inclusive in [
            0,
            epoch_schedule.slots_per_epoch,
            epoch_schedule.slots_per_epoch * 2,
            epoch_schedule.slots_per_epoch * 10,
        ] {
            db.add_root(max_root_inclusive);
            // oldest non-ancient will never exceed max_root_inclusive, even if the offset is so large it would mathematically move ancient PAST the newest root
            assert_eq!(
                max_root_inclusive,
                db.get_oldest_non_ancient_slot(&epoch_schedule)
            );
        }
    }
}

#[test]
fn test_sweep_get_oldest_non_ancient_slot() {
    let epoch_schedule = EpochSchedule::default();
    let ancient_append_vec_offset = 50_000;
    let db = AccountsDb::new_with_config(
        Vec::new(),
        Some(AccountsDbConfig {
            ancient_append_vec_offset: Some(ancient_append_vec_offset),
            ..ACCOUNTS_DB_CONFIG_FOR_TESTING
        }),
        None,
        Arc::default(),
    );
    // before any roots are added, we expect the oldest non-ancient slot to be 0
    assert_eq!(0, db.get_oldest_non_ancient_slot(&epoch_schedule));
    // adding roots until slots_per_epoch +/- ancient_append_vec_offset should still saturate to 0 as oldest non ancient slot
    let max_root_inclusive = AccountsDb::apply_offset_to_slot(0, ancient_append_vec_offset - 1);
    db.add_root(max_root_inclusive);
    // oldest non-ancient will never exceed max_root_inclusive
    assert_eq!(0, db.get_oldest_non_ancient_slot(&epoch_schedule));
    for offset in 0..3u64 {
        let max_root_inclusive = ancient_append_vec_offset as u64 + offset;
        db.add_root(max_root_inclusive);
        assert_eq!(
            0,
            db.get_oldest_non_ancient_slot(&epoch_schedule),
            "offset: {offset}"
        );
    }
    for offset in 0..3u64 {
        let max_root_inclusive = AccountsDb::apply_offset_to_slot(
            epoch_schedule.slots_per_epoch - 1,
            -ancient_append_vec_offset,
        ) + offset;
        db.add_root(max_root_inclusive);
        assert_eq!(
            offset,
            db.get_oldest_non_ancient_slot(&epoch_schedule),
            "offset: {offset}, max_root_inclusive: {max_root_inclusive}"
        );
    }
}

#[test]
fn test_sweep_get_oldest_non_ancient_slot2() {
    // note that this test has to worry about saturation at 0 as we subtract `slots_per_epoch` and `ancient_append_vec_offset`
    let epoch_schedule = EpochSchedule::default();
    for ancient_append_vec_offset in [-10_000i64, 50_000] {
        // at `starting_slot_offset`=0, with a negative `ancient_append_vec_offset`, we expect saturation to 0
        // big enough to avoid all saturation issues.
        let avoid_saturation = 1_000_000;
        assert!(
            avoid_saturation
                > epoch_schedule.slots_per_epoch + ancient_append_vec_offset.unsigned_abs()
        );
        for starting_slot_offset in [0, avoid_saturation] {
            let db = AccountsDb::new_with_config(
                Vec::new(),
                Some(AccountsDbConfig {
                    ancient_append_vec_offset: Some(ancient_append_vec_offset),
                    ..ACCOUNTS_DB_CONFIG_FOR_TESTING
                }),
                None,
                Arc::default(),
            );
            // before any roots are added, we expect the oldest non-ancient slot to be 0
            assert_eq!(0, db.get_oldest_non_ancient_slot(&epoch_schedule));

            let ancient_append_vec_offset = db.ancient_append_vec_offset.unwrap();
            assert_ne!(ancient_append_vec_offset, 0);
            // try a few values to simulate a real validator
            for inc in [0, 1, 2, 3, 4, 5, 8, 10, 10, 11, 200, 201, 1_000] {
                // oldest non-ancient slot is 1 greater than first ancient slot
                let completed_slot = epoch_schedule.slots_per_epoch + inc + starting_slot_offset;

                // test get_oldest_non_ancient_slot, which is based off the largest root
                db.add_root(completed_slot);
                let expected_oldest_non_ancient_slot = AccountsDb::apply_offset_to_slot(
                    AccountsDb::apply_offset_to_slot(
                        completed_slot,
                        -((epoch_schedule.slots_per_epoch as i64).saturating_sub(1)),
                    ),
                    ancient_append_vec_offset,
                );
                assert_eq!(
                    expected_oldest_non_ancient_slot,
                    db.get_oldest_non_ancient_slot(&epoch_schedule)
                );
            }
        }
    }
}

define_accounts_db_test!(test_get_sorted_potential_ancient_slots, |db| {
    let ancient_append_vec_offset = db.ancient_append_vec_offset.unwrap();
    let epoch_schedule = EpochSchedule::default();
    let oldest_non_ancient_slot = db.get_oldest_non_ancient_slot(&epoch_schedule);
    assert!(db
        .get_sorted_potential_ancient_slots(oldest_non_ancient_slot)
        .is_empty());
    let root1 = DEFAULT_MAX_ANCIENT_STORAGES as u64 + ancient_append_vec_offset as u64 + 1;
    db.add_root(root1);
    let root2 = root1 + 1;
    db.add_root(root2);
    let oldest_non_ancient_slot = db.get_oldest_non_ancient_slot(&epoch_schedule);
    assert!(db
        .get_sorted_potential_ancient_slots(oldest_non_ancient_slot)
        .is_empty());
    let completed_slot = epoch_schedule.slots_per_epoch;
    db.accounts_index.add_root(AccountsDb::apply_offset_to_slot(
        completed_slot,
        ancient_append_vec_offset,
    ));
    let oldest_non_ancient_slot = db.get_oldest_non_ancient_slot(&epoch_schedule);
    // get_sorted_potential_ancient_slots uses 'less than' as opposed to 'less or equal'
    // so, we need to get more than an epoch away to get the first valid root
    assert!(db
        .get_sorted_potential_ancient_slots(oldest_non_ancient_slot)
        .is_empty());
    let completed_slot = epoch_schedule.slots_per_epoch + root1;
    db.accounts_index.add_root(AccountsDb::apply_offset_to_slot(
        completed_slot,
        ancient_append_vec_offset,
    ));
    let oldest_non_ancient_slot = db.get_oldest_non_ancient_slot(&epoch_schedule);
    assert_eq!(
        db.get_sorted_potential_ancient_slots(oldest_non_ancient_slot),
        vec![root1, root2]
    );
    let completed_slot = epoch_schedule.slots_per_epoch + root2;
    db.accounts_index.add_root(AccountsDb::apply_offset_to_slot(
        completed_slot,
        ancient_append_vec_offset,
    ));
    let oldest_non_ancient_slot = db.get_oldest_non_ancient_slot(&epoch_schedule);
    assert_eq!(
        db.get_sorted_potential_ancient_slots(oldest_non_ancient_slot),
        vec![root1, root2]
    );
    db.accounts_index
        .roots_tracker
        .write()
        .unwrap()
        .alive_roots
        .remove(&root1);
    let oldest_non_ancient_slot = db.get_oldest_non_ancient_slot(&epoch_schedule);
    assert_eq!(
        db.get_sorted_potential_ancient_slots(oldest_non_ancient_slot),
        vec![root2]
    );
});

#[test]
fn test_shrink_collect_simple() {
    solana_logger::setup();
    let account_counts = [
        1,
        SHRINK_COLLECT_CHUNK_SIZE,
        SHRINK_COLLECT_CHUNK_SIZE + 1,
        SHRINK_COLLECT_CHUNK_SIZE * 2,
    ];
    // 2 = append_opposite_alive_account + append_opposite_zero_lamport_account
    let max_appended_accounts = 2;
    let max_num_accounts = *account_counts.iter().max().unwrap();
    let pubkeys = (0..(max_num_accounts + max_appended_accounts))
        .map(|_| solana_pubkey::new_rand())
        .collect::<Vec<_>>();
    // write accounts, maybe remove from index
    // check shrink_collect results
    for lamports in [0, 1] {
        for space in [0, 8] {
            if lamports == 0 && space != 0 {
                // illegal - zero lamport accounts are written with 0 space
                continue;
            }
            for alive in [false, true] {
                for append_opposite_alive_account in [false, true] {
                    for append_opposite_zero_lamport_account in [true, false] {
                        for mut account_count in account_counts {
                            let mut normal_account_count = account_count;
                            let mut pubkey_opposite_zero_lamports = None;
                            if append_opposite_zero_lamport_account {
                                pubkey_opposite_zero_lamports = Some(&pubkeys[account_count]);
                                normal_account_count += 1;
                                account_count += 1;
                            }
                            let mut pubkey_opposite_alive = None;
                            if append_opposite_alive_account {
                                // this needs to happen AFTER append_opposite_zero_lamport_account
                                pubkey_opposite_alive = Some(&pubkeys[account_count]);
                                account_count += 1;
                            }
                            debug!(
                                "space: {space}, lamports: {lamports}, alive: {alive}, \
                                 account_count: {account_count}, append_opposite_alive_account: \
                                 {append_opposite_alive_account}, \
                                 append_opposite_zero_lamport_account: \
                                 {append_opposite_zero_lamport_account}, normal_account_count: \
                                 {normal_account_count}"
                            );
                            let db = AccountsDb::new_single_for_tests();
                            let slot5 = 5;
                            // don't do special zero lamport account handling
                            db.set_latest_full_snapshot_slot(0);
                            let mut account = AccountSharedData::new(
                                lamports,
                                space,
                                AccountSharedData::default().owner(),
                            );
                            let mut to_purge = Vec::default();
                            for pubkey in pubkeys.iter().take(account_count) {
                                // store in append vec and index
                                let old_lamports = account.lamports();
                                if Some(pubkey) == pubkey_opposite_zero_lamports {
                                    account.set_lamports(u64::from(old_lamports == 0));
                                }

                                db.store_for_tests((slot5, [(pubkey, &account)].as_slice()));
                                account.set_lamports(old_lamports);
                                let mut alive = alive;
                                if append_opposite_alive_account
                                    && Some(pubkey) == pubkey_opposite_alive
                                {
                                    // invert this for one special pubkey
                                    alive = !alive;
                                }
                                if !alive {
                                    // remove from index so pubkey is 'dead'
                                    to_purge.push(*pubkey);
                                }
                            }
                            db.add_root_and_flush_write_cache(slot5);
                            to_purge.iter().for_each(|pubkey| {
                                db.accounts_index.purge_exact(
                                    pubkey,
                                    &([slot5].into_iter().collect::<HashSet<_>>()),
                                    &mut Vec::default(),
                                );
                            });

                            let storage = db.get_storage_for_slot(slot5).unwrap();
                            let mut unique_accounts = db
                                .get_unique_accounts_from_storage_for_shrink(
                                    &storage,
                                    &ShrinkStats::default(),
                                );

                            let shrink_collect = db.shrink_collect::<AliveAccounts<'_>>(
                                &storage,
                                &mut unique_accounts,
                                &ShrinkStats::default(),
                            );
                            let expect_single_opposite_alive_account =
                                if append_opposite_alive_account {
                                    vec![*pubkey_opposite_alive.unwrap()]
                                } else {
                                    vec![]
                                };

                            let expected_alive_accounts = if alive {
                                pubkeys[..normal_account_count]
                                    .iter()
                                    .filter(|p| Some(p) != pubkey_opposite_alive.as_ref())
                                    .sorted()
                                    .cloned()
                                    .collect::<Vec<_>>()
                            } else {
                                expect_single_opposite_alive_account.clone()
                            };

                            let expected_unrefed = if alive {
                                expect_single_opposite_alive_account.clone()
                            } else {
                                pubkeys[..normal_account_count]
                                    .iter()
                                    .sorted()
                                    .cloned()
                                    .collect::<Vec<_>>()
                            };

                            assert_eq!(shrink_collect.slot, slot5);

                            assert_eq!(
                                shrink_collect
                                    .alive_accounts
                                    .accounts
                                    .iter()
                                    .map(|account| *account.pubkey())
                                    .sorted()
                                    .collect::<Vec<_>>(),
                                expected_alive_accounts
                            );
                            assert_eq!(
                                shrink_collect
                                    .pubkeys_to_unref
                                    .iter()
                                    .sorted()
                                    .cloned()
                                    .cloned()
                                    .collect::<Vec<_>>(),
                                expected_unrefed
                            );

                            let alive_total_one_account = 136 + space;
                            if alive {
                                let mut expected_alive_total_bytes =
                                    alive_total_one_account * normal_account_count;
                                if append_opposite_zero_lamport_account {
                                    // zero lamport accounts store size=0 data
                                    expected_alive_total_bytes -= space;
                                }
                                assert_eq!(
                                    shrink_collect.alive_total_bytes,
                                    expected_alive_total_bytes
                                );
                            } else if append_opposite_alive_account {
                                assert_eq!(
                                    shrink_collect.alive_total_bytes,
                                    alive_total_one_account
                                );
                            } else {
                                assert_eq!(shrink_collect.alive_total_bytes, 0);
                            }
                            // expected_capacity is determined by what size append vec gets created when the write cache is flushed to an append vec.
                            let mut expected_capacity =
                                (account_count * aligned_stored_size(space)) as u64;
                            if append_opposite_zero_lamport_account && space != 0 {
                                // zero lamport accounts always write space = 0
                                expected_capacity -= space as u64;
                            }

                            assert_eq!(shrink_collect.capacity, expected_capacity);
                            assert_eq!(shrink_collect.total_starting_accounts, account_count);
                            let mut expected_all_are_zero_lamports = lamports == 0;
                            if !append_opposite_alive_account {
                                expected_all_are_zero_lamports |= !alive;
                            }
                            if append_opposite_zero_lamport_account && lamports == 0 && alive {
                                expected_all_are_zero_lamports = !expected_all_are_zero_lamports;
                            }
                            assert_eq!(
                                shrink_collect.all_are_zero_lamports,
                                expected_all_are_zero_lamports
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn test_shrink_collect_with_obsolete_accounts() {
    solana_logger::setup();
    let account_count = 100;
    let pubkeys: Vec<_> = iter::repeat_with(Pubkey::new_unique)
        .take(account_count)
        .collect();

    let db = AccountsDb::new_single_for_tests();
    let slot = 5;

    let mut account = AccountSharedData::new(
        100, // lamports
        128, // space
        AccountSharedData::default().owner(),
    );

    let mut regular_pubkeys = Vec::new();
    let mut obsolete_pubkeys = Vec::new();
    let mut zero_lamport_pubkeys = Vec::new();
    let mut unref_pubkeys = Vec::new();

    for (i, pubkey) in pubkeys.iter().enumerate() {
        if i % 3 == 0 {
            // Mark third account as zero lamport
            // These will be removed during shrink
            account.set_lamports(0);
            zero_lamport_pubkeys.push(*pubkey);
        } else {
            // Regular accounts that should be kept
            account.set_lamports(200);
            regular_pubkeys.push(*pubkey);
        }
        db.store_for_tests((slot, [(pubkey, &account)].as_slice()));
    }

    // Flush the cache
    db.add_root_and_flush_write_cache(slot);

    let storage = db.get_and_assert_single_storage(slot);

    for (i, pubkey) in pubkeys.iter().enumerate() {
        // Mark Some accounts obsolete. These will include zero lamport and non zero lamport accounts
        if i % 5 == 0 {
            // Lookup the pubkey in the database and find the AccountInfo
            db.accounts_index
                .get_with_and_then(pubkey, None, None, false, |account_info| {
                    db.remove_dead_accounts(
                        [account_info].iter(),
                        None,
                        MarkAccountsObsolete::Yes(slot),
                    );
                });

            obsolete_pubkeys.push(*pubkey);
        } else if i % 4 == 0 {
            // Purge accounts via clean and ensure that they will be unreffed.
            db.accounts_index.purge_exact(
                pubkey,
                &([slot].into_iter().collect::<HashSet<_>>()),
                &mut Vec::default(),
            );
            unref_pubkeys.push(*pubkey);
        }
    }

    let mut unique_accounts =
        db.get_unique_accounts_from_storage_for_shrink(&storage, &ShrinkStats::default());

    let shrink_collect = db.shrink_collect::<AliveAccounts<'_>>(
        &storage,
        &mut unique_accounts,
        &ShrinkStats::default(),
    );

    assert_eq!(shrink_collect.slot, slot);

    // Ensure that the keys to unref does not include the obsolete accounts and only includes the unreferenced accounts
    assert_eq!(
        shrink_collect
            .pubkeys_to_unref
            .into_iter()
            .collect::<HashSet<_>>(),
        unref_pubkeys.iter().clone().collect::<HashSet<_>>()
    );

    // Ensure that the obsolete accounts and accounts to unref are not in the alive list
    assert_eq!(
        shrink_collect
            .alive_accounts
            .accounts
            .into_iter()
            .map(|account| *account.pubkey())
            .sorted()
            .collect::<Vec<Pubkey>>(),
        regular_pubkeys
            .into_iter()
            .filter(|account| !unref_pubkeys.contains(account))
            .filter(|account| !obsolete_pubkeys.contains(account))
            .sorted()
            .collect::<Vec<Pubkey>>()
    );
}

pub(crate) const CAN_RANDOMLY_SHRINK_FALSE: bool = false;

define_accounts_db_test!(test_combine_ancient_slots_empty, |db| {
    // empty slots
    db.combine_ancient_slots_packed(Vec::default(), CAN_RANDOMLY_SHRINK_FALSE);
});

#[test]
fn test_combine_ancient_slots_simple() {
    _ = get_one_ancient_append_vec_and_others(0);
}

fn get_all_accounts_from_storages<'a>(
    storages: impl Iterator<Item = &'a Arc<AccountStorageEntry>>,
) -> Vec<(Pubkey, AccountSharedData)> {
    let mut reader = append_vec::new_scan_accounts_reader();
    storages
        .flat_map(|storage| {
            let mut vec = Vec::default();
            storage
                .accounts
                .scan_accounts(&mut reader, |_offset, account| {
                    vec.push((*account.pubkey(), account.to_account_shared_data()));
                })
                .expect("must scan accounts storage");
            // make sure scan_pubkeys results match
            // Note that we assume traversals are both in the same order, but this doesn't have to be true.
            let mut compare = Vec::default();
            storage
                .accounts
                .scan_pubkeys(|k| {
                    compare.push(*k);
                })
                .expect("must scan accounts storage");
            assert_eq!(compare, vec.iter().map(|(k, _)| *k).collect::<Vec<_>>());
            vec
        })
        .collect::<Vec<_>>()
}

pub(crate) fn get_all_accounts(
    db: &AccountsDb,
    slots: impl Iterator<Item = Slot>,
) -> Vec<(Pubkey, AccountSharedData)> {
    slots
        .filter_map(|slot| {
            let storage = db.storage.get_slot_storage_entry(slot);
            storage.map(|storage| get_all_accounts_from_storages(std::iter::once(&storage)))
        })
        .flatten()
        .collect::<Vec<_>>()
}

#[track_caller]
pub(crate) fn compare_all_accounts(
    one: &[(Pubkey, AccountSharedData)],
    two: &[(Pubkey, AccountSharedData)],
) {
    let mut failures = 0;
    let mut two_indexes = (0..two.len()).collect::<Vec<_>>();
    one.iter().for_each(|(pubkey, account)| {
        for i in 0..two_indexes.len() {
            let pubkey2 = two[two_indexes[i]].0;
            if pubkey2 == *pubkey {
                if !accounts_equal(account, &two[two_indexes[i]].1) {
                    failures += 1;
                }
                two_indexes.remove(i);
                break;
            }
        }
    });
    // helper method to reduce the volume of logged data to help identify differences
    // modify this when you hit a failure
    let clean = |accounts: &[(Pubkey, AccountSharedData)]| {
        accounts
            .iter()
            .map(|(_pubkey, account)| account.lamports())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        failures,
        0,
        "one: {:?}, two: {:?}, two_indexes: {:?}",
        clean(one),
        clean(two),
        two_indexes,
    );
    assert!(
        two_indexes.is_empty(),
        "one: {one:?}, two: {two:?}, two_indexes: {two_indexes:?}"
    );
}

#[test]
fn test_shrink_ancient_overflow_with_min_size() {
    solana_logger::setup();

    let ideal_av_size = ancient_append_vecs::get_ancient_append_vec_capacity();
    let num_normal_slots = 2;

    // build an ancient append vec at slot 'ancient_slot' with one `fat`
    // account that's larger than the ideal size of ancient append vec to
    // simulate the *oversized* append vec for shrinking.
    let account_size = (1.5 * ideal_av_size as f64) as u64;
    let (db, ancient_slot) = get_one_ancient_append_vec_and_others_with_account_size(
        num_normal_slots,
        Some(account_size),
    );

    let max_slot_inclusive = ancient_slot + (num_normal_slots as Slot);
    let initial_accounts = get_all_accounts(&db, ancient_slot..(max_slot_inclusive + 1));

    let ancient = db.storage.get_slot_storage_entry(ancient_slot).unwrap();

    // assert that the min_size, which about 1.5 * ideal_av_size, kicked in
    // and result that the ancient append vec capacity exceeds the ideal_av_size
    assert!(ancient.capacity() > ideal_av_size);

    // combine 1 normal append vec into existing oversize ancient append vec.
    db.combine_ancient_slots_packed(
        (ancient_slot..max_slot_inclusive).collect(),
        CAN_RANDOMLY_SHRINK_FALSE,
    );

    compare_all_accounts(
        &initial_accounts,
        &get_all_accounts(&db, ancient_slot..max_slot_inclusive),
    );

    // the append vec at max_slot_inclusive-1 should NOT have been removed
    // since the append vec is already oversized and we created an ancient
    // append vec there.
    let ancient2 = db
        .storage
        .get_slot_storage_entry(max_slot_inclusive - 1)
        .unwrap();
    assert!(ancient2.capacity() > ideal_av_size); // min_size kicked in, which cause the appendvec to be larger than the ideal_av_size

    // Combine normal append vec(s) into existing ancient append vec this
    // will overflow the original ancient append vec because of the oversized
    // ancient append vec is full.
    db.combine_ancient_slots_packed(
        (ancient_slot..=max_slot_inclusive).collect(),
        CAN_RANDOMLY_SHRINK_FALSE,
    );

    compare_all_accounts(
        &initial_accounts,
        &get_all_accounts(&db, ancient_slot..(max_slot_inclusive + 1)),
    );

    // Nothing should be combined because the append vec are oversized.
    // min_size kicked in, which cause the appendvecs to be larger than the ideal_av_size.
    let ancient = db.storage.get_slot_storage_entry(ancient_slot).unwrap();
    assert!(ancient.capacity() > ideal_av_size);

    let ancient2 = db
        .storage
        .get_slot_storage_entry(max_slot_inclusive - 1)
        .unwrap();
    assert!(ancient2.capacity() > ideal_av_size);

    let ancient3 = db
        .storage
        .get_slot_storage_entry(max_slot_inclusive)
        .unwrap();
    assert!(ancient3.capacity() > ideal_av_size);
}

#[test]
fn test_shrink_overflow_too_much() {
    let num_normal_slots = 2;
    let ideal_av_size = ancient_append_vecs::get_ancient_append_vec_capacity();
    let fat_account_size = (1.5 * ideal_av_size as f64) as u64;

    // Prepare 3 append vecs to combine [small, big, small]
    let account_data_sizes = vec![100, fat_account_size, 100];
    let (db, slot1) = create_db_with_storages_and_index_with_customized_account_size_per_slot(
        true,
        num_normal_slots + 1,
        account_data_sizes,
    );
    let storage = db.get_storage_for_slot(slot1).unwrap();
    let created_accounts = db.get_unique_accounts_from_storage(&storage);

    // Adjust alive_ratio for slot2 to test it is shrinkable and is a
    // candidate for squashing into the previous ancient append vec.
    // However, due to the fact that this append vec is `oversized`, it can't
    // be squashed into the ancient append vec at previous slot (exceeds the
    // size limit). Therefore, a new "oversized" ancient append vec is
    // created at slot2 as the overflow. This is where the "min_bytes" in
    // `fn create_ancient_append_vec` is used.
    let slot2 = slot1 + 1;
    let storage2 = db.storage.get_slot_storage_entry(slot2).unwrap();
    let original_cap_slot2 = storage2.accounts.capacity();
    storage2
        .accounts
        .set_current_len_for_tests(original_cap_slot2 as usize);

    // Combine append vec into ancient append vec.
    let slots_to_combine: Vec<Slot> = (slot1..slot1 + (num_normal_slots + 1) as Slot).collect();
    db.combine_ancient_slots_packed(slots_to_combine, CAN_RANDOMLY_SHRINK_FALSE);

    // slot2 is too big to fit into ideal ancient append vec at slot1. So slot2 won't be merged into slot1.
    // slot1 will have its own ancient append vec.
    assert!(db.storage.get_slot_storage_entry(slot1).is_some());
    let ancient = db.get_storage_for_slot(slot1).unwrap();
    assert!(ancient.capacity() <= ideal_av_size);

    let after_store = db.get_storage_for_slot(slot1).unwrap();
    let GetUniqueAccountsResult {
        stored_accounts: after_stored_accounts,
        capacity: after_capacity,
        ..
    } = db.get_unique_accounts_from_storage(&after_store);
    assert!(created_accounts.capacity <= after_capacity);
    assert_eq!(created_accounts.stored_accounts.len(), 1);
    assert_eq!(after_stored_accounts.len(), 1);

    // slot2, even after shrinking, is still oversized. Therefore, slot 2
    // exists as an ancient append vec.
    let storage2_after = db.storage.get_slot_storage_entry(slot2).unwrap();
    assert!(storage2_after.capacity() > ideal_av_size);
    let after_store = db.get_storage_for_slot(slot2).unwrap();
    let GetUniqueAccountsResult {
        stored_accounts: after_stored_accounts,
        capacity: after_capacity,
        ..
    } = db.get_unique_accounts_from_storage(&after_store);
    assert!(created_accounts.capacity <= after_capacity);
    assert_eq!(created_accounts.stored_accounts.len(), 1);
    assert_eq!(after_stored_accounts.len(), 1);
}

pub fn get_account_from_account_from_storage(
    account: &AccountFromStorage,
    db: &AccountsDb,
    slot: Slot,
) -> AccountSharedData {
    let storage = db
        .storage
        .get_slot_storage_entry_shrinking_in_progress_ok(slot)
        .unwrap();
    storage
        .accounts
        .get_account_shared_data(account.index_info.offset())
        .unwrap()
}

fn populate_index(db: &AccountsDb, slots: Range<Slot>) {
    slots.into_iter().for_each(|slot| {
        if let Some(storage) = db.get_storage_for_slot(slot) {
            storage
                .accounts
                .scan_accounts_stored_meta(|account| {
                    let info = AccountInfo::new(
                        StorageLocation::AppendVec(storage.id(), account.offset()),
                        account.is_zero_lamport(),
                    );
                    db.accounts_index.upsert(
                        slot,
                        slot,
                        account.pubkey(),
                        &account,
                        &AccountSecondaryIndexes::default(),
                        info,
                        &mut Vec::default(),
                        UpsertReclaim::IgnoreReclaims,
                    );
                })
                .expect("must scan accounts storage");
        }
    })
}

pub(crate) fn remove_account_for_tests(storage: &AccountStorageEntry, num_bytes: usize) {
    storage.remove_accounts(num_bytes, 1);
}

pub(crate) fn create_storages_and_update_index_with_customized_account_size_per_slot(
    db: &AccountsDb,
    tf: Option<&TempFile>,
    starting_slot: Slot,
    num_slots: usize,
    alive: bool,
    account_data_sizes: Vec<u64>,
) {
    if num_slots == 0 {
        return;
    }
    assert!(account_data_sizes.len() == num_slots);
    let local_tf = (tf.is_none()).then(|| {
        crate::append_vec::test_utils::get_append_vec_path("create_storages_and_update_index")
    });
    let tf = tf.unwrap_or_else(|| local_tf.as_ref().unwrap());

    let starting_id = db
        .storage
        .iter()
        .map(|storage| storage.1.id())
        .max()
        .unwrap_or(999);
    for (i, account_data_size) in account_data_sizes.iter().enumerate().take(num_slots) {
        let id = starting_id + (i as AccountsFileId);
        let pubkey1 = solana_pubkey::new_rand();
        let storage = sample_storage_with_entries_id_fill_percentage(
            tf,
            starting_slot + (i as Slot),
            &pubkey1,
            id,
            alive,
            Some(*account_data_size),
            50,
        );
        insert_store(db, Arc::clone(&storage));
    }

    let storage = db.get_storage_for_slot(starting_slot).unwrap();
    let created_accounts = db.get_unique_accounts_from_storage(&storage);
    assert_eq!(created_accounts.stored_accounts.len(), 1);

    if alive {
        populate_index(db, starting_slot..(starting_slot + (num_slots as Slot) + 1));
    }
}

pub(crate) fn create_storages_and_update_index(
    db: &AccountsDb,
    tf: Option<&TempFile>,
    starting_slot: Slot,
    num_slots: usize,
    alive: bool,
    account_data_size: Option<u64>,
) {
    if num_slots == 0 {
        return;
    }

    let local_tf = (tf.is_none()).then(|| {
        crate::append_vec::test_utils::get_append_vec_path("create_storages_and_update_index")
    });
    let tf = tf.unwrap_or_else(|| local_tf.as_ref().unwrap());

    let starting_id = db
        .storage
        .iter()
        .map(|storage| storage.1.id())
        .max()
        .unwrap_or(999);
    for i in 0..num_slots {
        let id = starting_id + (i as AccountsFileId);
        let pubkey1 = solana_pubkey::new_rand();
        let storage = sample_storage_with_entries_id(
            tf,
            starting_slot + (i as Slot),
            &pubkey1,
            id,
            alive,
            account_data_size,
        );
        insert_store(db, Arc::clone(&storage));
    }

    let storage = db.get_storage_for_slot(starting_slot).unwrap();
    let created_accounts = db.get_unique_accounts_from_storage(&storage);
    assert_eq!(created_accounts.stored_accounts.len(), 1);

    if alive {
        populate_index(db, starting_slot..(starting_slot + (num_slots as Slot) + 1));
    }
}

pub(crate) fn create_db_with_storages_and_index(
    alive: bool,
    num_slots: usize,
    account_data_size: Option<u64>,
) -> (AccountsDb, Slot) {
    solana_logger::setup();

    let db = AccountsDb::new_single_for_tests();

    // create a single append vec with a single account in a slot
    // add the pubkey to index if alive
    // call combine_ancient_slots with the slot
    // verify we create an ancient appendvec that has alive accounts and does not have dead accounts

    let slot1 = 1;
    create_storages_and_update_index(&db, None, slot1, num_slots, alive, account_data_size);

    let slot1 = slot1 as Slot;
    (db, slot1)
}

pub(crate) fn create_db_with_storages_and_index_with_customized_account_size_per_slot(
    alive: bool,
    num_slots: usize,
    account_data_size: Vec<u64>,
) -> (AccountsDb, Slot) {
    solana_logger::setup();

    let db = AccountsDb::new_single_for_tests();

    // create a single append vec with a single account in a slot
    // add the pubkey to index if alive
    // call combine_ancient_slots with the slot
    // verify we create an ancient appendvec that has alive accounts and does not have dead accounts

    let slot1 = 1;
    create_storages_and_update_index_with_customized_account_size_per_slot(
        &db,
        None,
        slot1,
        num_slots,
        alive,
        account_data_size,
    );

    let slot1 = slot1 as Slot;
    (db, slot1)
}

fn get_one_ancient_append_vec_and_others_with_account_size(
    num_normal_slots: usize,
    account_data_size: Option<u64>,
) -> (AccountsDb, Slot) {
    // We used to test 'alive = false' with the old shrinking algorithm, but
    // not any more with the new shrinking algorithm. 'alive = false' means
    // that we will have account entries that's in the storages but not in
    // accounts-db index. This violate the assumption in accounts-db, which
    // the new shrinking algorithm now depends on. Therefore, we don't test
    // 'alive = false'.
    let alive = true;
    let (db, slot1) =
        create_db_with_storages_and_index(alive, num_normal_slots + 1, account_data_size);
    let storage = db.get_storage_for_slot(slot1).unwrap();
    let created_accounts = db.get_unique_accounts_from_storage(&storage);

    db.combine_ancient_slots_packed(vec![slot1], CAN_RANDOMLY_SHRINK_FALSE);
    let after_store = db.get_storage_for_slot(slot1).unwrap();
    let GetUniqueAccountsResult {
        stored_accounts: after_stored_accounts,
        capacity: after_capacity,
        ..
    } = db.get_unique_accounts_from_storage(&after_store);
    assert!(created_accounts.capacity <= after_capacity);
    assert_eq!(created_accounts.stored_accounts.len(), 1);
    // always 1 account: either we leave the append vec alone if it is all dead
    // or we create a new one and copy into it if account is alive
    assert_eq!(after_stored_accounts.len(), 1);
    (db, slot1)
}

fn get_one_ancient_append_vec_and_others(num_normal_slots: usize) -> (AccountsDb, Slot) {
    get_one_ancient_append_vec_and_others_with_account_size(num_normal_slots, None)
}

#[test]
fn test_handle_dropped_roots_for_ancient() {
    solana_logger::setup();
    let db = AccountsDb::new_single_for_tests();
    db.handle_dropped_roots_for_ancient(std::iter::empty::<Slot>());
    let slot0 = 0;
    let dropped_roots = vec![slot0];
    db.accounts_index.add_root(slot0);
    assert!(db.accounts_index.is_alive_root(slot0));
    db.handle_dropped_roots_for_ancient(dropped_roots.into_iter());
    assert!(!db.accounts_index.is_alive_root(slot0));
}

fn insert_store(db: &AccountsDb, append_vec: Arc<AccountStorageEntry>) {
    db.storage.insert(append_vec.slot(), append_vec);
}

#[test]
#[should_panic(expected = "self.storage.remove")]
fn test_handle_dropped_roots_for_ancient_assert() {
    solana_logger::setup();
    let common_store_path = Path::new("");
    let store_file_size = 10_000;
    let entry = Arc::new(AccountStorageEntry::new(
        common_store_path,
        0,
        1,
        store_file_size,
        AccountsFileProvider::AppendVec,
    ));
    let db = AccountsDb::new_single_for_tests();
    let slot0 = 0;
    let dropped_roots = vec![slot0];
    insert_store(&db, entry);
    db.handle_dropped_roots_for_ancient(dropped_roots.into_iter());
}

/// Test that `clean` reclaims old accounts when cleaning old storages
///
/// When `clean` constructs candidates from old storages, pubkeys in these storages may have other
/// newer versions of the accounts in other newer storages *not* explicitly marked to be visited by
/// `clean`.  In this case, `clean` should still reclaim the old versions of these accounts.
#[test]
fn test_clean_old_storages_with_reclaims_rooted() {
    let accounts_db = AccountsDb::new_single_for_tests();
    let pubkey = Pubkey::new_unique();
    let old_slot = 11;
    let new_slot = 22;
    let slots = [old_slot, new_slot];
    for &slot in &slots {
        let account = AccountSharedData::new(slot, 0, &Pubkey::new_unique());
        // store `pubkey` into multiple slots, and also store another unique pubkey
        // to prevent the whole storage from being marked as dead by `clean`.
        accounts_db.store_for_tests((
            slot,
            [(&pubkey, &account), (&Pubkey::new_unique(), &account)].as_slice(),
        ));
        accounts_db.add_root_and_flush_write_cache(slot);
        accounts_db.uncleaned_pubkeys.remove(&slot);
        // ensure this slot is *not* in the dirty_stores nor uncleaned_pubkeys, because we want to
        // test cleaning *old* storages, i.e. when they aren't explicitly marked for cleaning
        assert!(!accounts_db.dirty_stores.contains_key(&slot));
        assert!(!accounts_db.uncleaned_pubkeys.contains_key(&slot));
    }

    // add `old_slot` to the dirty stores list to mimic it being picked up as old
    let old_storage = accounts_db
        .storage
        .get_slot_storage_entry_shrinking_in_progress_ok(old_slot)
        .unwrap();
    accounts_db.dirty_stores.insert(old_slot, old_storage);

    // ensure the slot list for `pubkey` has both the old and new slots
    let slot_list = accounts_db
        .accounts_index
        .get_bin(&pubkey)
        .slot_list_mut(&pubkey, |slot_list| slot_list.clone())
        .unwrap();
    assert_eq!(slot_list.len(), slots.len());
    assert!(slot_list.iter().map(|(slot, _)| slot).eq(slots.iter()));

    // `clean` should now reclaim the account in `old_slot`, even though `new_slot` is not
    // explicitly being cleaned
    accounts_db.clean_accounts_for_tests();

    // ensure we've reclaimed the account in `old_slot`
    let slot_list = accounts_db
        .accounts_index
        .get_bin(&pubkey)
        .slot_list_mut(&pubkey, |slot_list| slot_list.clone())
        .unwrap();
    assert_eq!(slot_list.len(), 1);
    assert!(slot_list
        .iter()
        .map(|(slot, _)| slot)
        .eq(iter::once(&new_slot)));
}

/// Test that `clean` respects rooted vs unrooted slots w.r.t. reclaims
///
/// When an account is in multiple slots, and the latest is unrooted, `clean` should *not* reclaim
/// all the rooted versions.
#[test]
fn test_clean_old_storages_with_reclaims_unrooted() {
    let accounts_db = AccountsDb::new_single_for_tests();
    let pubkey = Pubkey::new_unique();
    let old_slot = 11;
    let new_slot = 22;
    let slots = [old_slot, new_slot];
    for &slot in &slots {
        let account = AccountSharedData::new(slot, 0, &Pubkey::new_unique());
        // store `pubkey` into multiple slots, and also store another unique pubkey
        // to prevent the whole storage from being marked as dead by `clean`.
        accounts_db.store_for_tests((
            slot,
            [(&pubkey, &account), (&Pubkey::new_unique(), &account)].as_slice(),
        ));
    }

    // only `old_slot` should be rooted, not `new_slot`
    accounts_db.add_root_and_flush_write_cache(old_slot);
    assert!(accounts_db.accounts_index.is_alive_root(old_slot));
    assert!(!accounts_db.accounts_index.is_alive_root(new_slot));

    // ensure `old_slot` is in uncleaned_pubkeys (but not dirty_stores) so it'll be cleaned
    assert!(accounts_db.uncleaned_pubkeys.contains_key(&old_slot));
    assert!(!accounts_db.dirty_stores.contains_key(&old_slot));
    // and `new_slot` should be in neither
    assert!(!accounts_db.uncleaned_pubkeys.contains_key(&new_slot));
    assert!(!accounts_db.dirty_stores.contains_key(&new_slot));

    // ensure the slot list for `pubkey` has both the old and new slots
    let slot_list = accounts_db
        .accounts_index
        .get_bin(&pubkey)
        .slot_list_mut(&pubkey, |slot_list| slot_list.clone())
        .unwrap();
    assert_eq!(slot_list.len(), slots.len());
    assert!(slot_list.iter().map(|(slot, _)| slot).eq(slots.iter()));

    // `clean` should *not* reclaim the account in `old_slot` because `new_slot` is not a root
    accounts_db.clean_accounts_for_tests();

    // ensure we have NOT reclaimed the account in `old_slot`
    let slot_list = accounts_db
        .accounts_index
        .get_bin(&pubkey)
        .slot_list_mut(&pubkey, |slot_list| slot_list.clone())
        .unwrap();
    assert_eq!(slot_list.len(), slots.len());
    assert!(slot_list.iter().map(|(slot, _)| slot).eq(slots.iter()));
}

/// Ensure the calculating capitalization produces the correct value
#[test]
fn test_calculate_capitalization_simple() {
    let accounts_db = AccountsDb::new_single_for_tests();
    accounts_db.store_for_tests((
        0,
        [(
            &Pubkey::new_unique(),
            &AccountSharedData::new(123, 0, &Pubkey::default()),
        )]
        .as_slice(),
    ));
    accounts_db.store_for_tests((
        1,
        [(
            &Pubkey::new_unique(),
            &AccountSharedData::new(456, 0, &Pubkey::default()),
        )]
        .as_slice(),
    ));
    assert_eq!(
        accounts_db.calculate_capitalization_at_startup_from_index(&Ancestors::from(vec![0, 1]), 1),
        123 + 456,
    );
}

/// Ensure that calculating capitalization panics of there is an overflow
/// while summing balance within a single slot.
#[test]
#[should_panic(expected = "capitalization cannot overflow")]
fn test_calculate_capitalization_overflow_intra_slot() {
    let accounts_db = AccountsDb::new_single_for_tests();
    let account = AccountSharedData::new(u64::MAX - 1, 0, &Pubkey::default());
    accounts_db.store_for_tests((0, [(&Pubkey::new_unique(), &account)].as_slice()));
    accounts_db.store_for_tests((0, [(&Pubkey::new_unique(), &account)].as_slice()));
    accounts_db.calculate_capitalization_at_startup_from_index(&Ancestors::from(vec![0]), 0);
}

/// Ensure that calculating capitalization panics of there is an overflow
/// while summing balance across multiple slots.
#[test]
#[should_panic(expected = "capitalization cannot overflow")]
fn test_calculate_capitalization_overflow_inter_slot() {
    let accounts_db = AccountsDb::new_single_for_tests();
    let account = AccountSharedData::new(u64::MAX - 1, 0, &Pubkey::default());
    accounts_db.store_for_tests((0, [(&Pubkey::new_unique(), &account)].as_slice()));
    accounts_db.store_for_tests((1, [(&Pubkey::new_unique(), &account)].as_slice()));
    accounts_db.calculate_capitalization_at_startup_from_index(&Ancestors::from(vec![0, 1]), 1);
}

#[test]
fn test_mark_obsolete_accounts_at_startup_none() {
    let (_accounts_dirs, paths) = get_temp_accounts_paths(2).unwrap();
    let accounts_db = AccountsDb::new_for_tests(paths);
    let slots = 0;
    let pubkeys_with_duplicates_by_bin = vec![];

    let obsolete_stats =
        accounts_db.mark_obsolete_accounts_at_startup(slots, pubkeys_with_duplicates_by_bin);

    assert_eq!(
        obsolete_stats.accounts_marked_obsolete, 0,
        "No accounts should be reclaimed for empty bin"
    );
}

#[test]
fn test_mark_obsolete_accounts_at_startup_purge_slot() {
    let (_accounts_dirs, paths) = get_temp_accounts_paths(2).unwrap();
    let accounts_db = AccountsDb::new_for_tests(paths);
    let slots = 2;
    let pubkey1 = Pubkey::new_unique();
    let pubkey2 = Pubkey::new_unique();
    let account = AccountSharedData::new(100, 0, &Pubkey::default());

    // Store the same pubkey in multiple slots
    // Store other pubkey in slot0 to ensure slot is not purged
    accounts_db.store_for_tests((0, [(&pubkey1, &account), (&pubkey2, &account)].as_slice()));
    accounts_db.flush_accounts_cache_slot_for_tests(0);
    accounts_db.store_for_tests((1, [(&pubkey1, &account)].as_slice()));
    accounts_db.flush_accounts_cache_slot_for_tests(1);
    accounts_db.store_for_tests((2, [(&pubkey1, &account)].as_slice()));
    accounts_db.flush_accounts_cache_slot_for_tests(2);

    let pubkeys_with_duplicates_by_bin = vec![vec![pubkey1]];

    let obsolete_stats =
        accounts_db.mark_obsolete_accounts_at_startup(slots, pubkeys_with_duplicates_by_bin);

    // Verify that slot 0 has not been purged
    assert!(accounts_db.storage.get_slot_storage_entry(0).is_some());

    // Verify that slot 1 has been purged
    assert!(accounts_db.storage.get_slot_storage_entry(1).is_none());

    // Verify that the pubkey ref1's count is 1
    assert_eq!(
        accounts_db.accounts_index.ref_count_from_storage(&pubkey1),
        1
    );

    assert_eq!(obsolete_stats.accounts_marked_obsolete, 2);
}

#[test]
fn test_mark_obsolete_accounts_at_startup_multiple_bins() {
    let (_accounts_dirs, paths) = get_temp_accounts_paths(2).unwrap();
    let accounts_db = AccountsDb::new_for_tests(paths);
    let pubkey1 = Pubkey::from([0; 32]); // Ensure pubkey1 is in bin 0
    let pubkey2 = Pubkey::from([255; 32]); // Ensure pubkey2 is in a different bin
    let account = AccountSharedData::new(100, 0, &Pubkey::default());

    for slot in 0..2 {
        accounts_db.store_for_tests((
            slot,
            [(&pubkey1, &account), (&pubkey2, &account)].as_slice(),
        ));
        accounts_db.flush_accounts_cache_slot_for_tests(slot);
    }

    let pubkeys_with_duplicates_by_bin = vec![vec![pubkey1], vec![pubkey2]];

    let obsolete_stats =
        accounts_db.mark_obsolete_accounts_at_startup(2, pubkeys_with_duplicates_by_bin);

    // Verify that slot 0 has been purged
    assert!(accounts_db.storage.get_slot_storage_entry(0).is_none());

    // Verify that slot 1 has been purged
    assert!(accounts_db.storage.get_slot_storage_entry(1).is_some());

    // Verify that both pubkeys ref_counts are 1
    assert_eq!(
        accounts_db.accounts_index.ref_count_from_storage(&pubkey1),
        1
    );
    assert_eq!(
        accounts_db.accounts_index.ref_count_from_storage(&pubkey2),
        1
    );

    // Ensure that stats were accumulated correctly
    assert_eq!(obsolete_stats.accounts_marked_obsolete, 2);
    assert_eq!(obsolete_stats.slots_removed, 1);
}

// This test verifies that when obsolete accounts are marked, the duplicates lt hash is set to the
// default value. When they are not marked, it is populated. The second case ensures test validity.
#[test_case(true; "mark_obsolete_accounts")]
#[test_case(false; "do_not_mark_obsolete_accounts")]
fn test_obsolete_accounts_empty_default_duplicate_hash(mark_obsolete_accounts: bool) {
    let slot0 = 0;
    let slot1 = 1;

    let db = AccountsDb::new_with_config(
        Vec::new(),
        Some(AccountsDbConfig {
            mark_obsolete_accounts,
            ..ACCOUNTS_DB_CONFIG_FOR_TESTING
        }),
        None,
        Arc::default(),
    );

    let pubkey = Pubkey::new_unique();

    let storage = db.create_and_insert_store(slot0, 1000, "test");

    let account0 = AccountSharedData::new(1, 0, &Pubkey::default());
    let account1 = AccountSharedData::new(100, 0, &Pubkey::default());

    storage
        .accounts
        .write_accounts(&(slot0, &[(&pubkey, &account0)][..]), 0);

    let storage = db.create_and_insert_store(slot1, 1000, "test");

    storage
        .accounts
        .write_accounts(&(slot1, &[(&pubkey, &account1)][..]), 0);

    assert!(!db.accounts_index.contains(&pubkey));
    let result = db.generate_index(None, false);
    if mark_obsolete_accounts {
        // If obsolete accounts are marked, the duplicates lt hash should be the default value
        // This is because all duplicates are marked as obsolete and skipped during lt hash calculation.
        assert_eq!(
            *result.duplicates_lt_hash.unwrap(),
            DuplicatesLtHash::default()
        );
    } else {
        assert_ne!(
            *result.duplicates_lt_hash.unwrap(),
            DuplicatesLtHash::default()
        );
    }
}

#[test]
fn test_batch_insert_zero_lamport_single_ref_account_offsets() {
    let accounts = AccountsDb::new_single_for_tests();
    let storage = accounts.create_and_insert_store(1, 100, "test");

    // Test inserting new offsets
    let offsets1 = vec![10, 20, 30];
    let count1 = storage.batch_insert_zero_lamport_single_ref_account_offsets(&offsets1);
    assert_eq!(count1, 3, "Should insert all 3 new offsets");
    assert_eq!(storage.num_zero_lamport_single_ref_accounts(), 3);

    // Test inserting some duplicate and some new offsets
    let offsets2 = vec![20, 30, 40, 50]; // 20,30 are duplicates, 40,50 are new
    let count2 = storage.batch_insert_zero_lamport_single_ref_account_offsets(&offsets2);
    assert_eq!(count2, 2, "Should insert only 2 new offsets (40, 50)");
    assert_eq!(storage.num_zero_lamport_single_ref_accounts(), 5);

    // Test inserting all duplicates
    let offsets3 = vec![10, 20];
    let count3 = storage.batch_insert_zero_lamport_single_ref_account_offsets(&offsets3);
    assert_eq!(count3, 0, "Should not insert any duplicates");
    assert_eq!(storage.num_zero_lamport_single_ref_accounts(), 5);

    // Test inserting empty slice
    let empty_offsets: Vec<usize> = vec![];
    let count4 = storage.batch_insert_zero_lamport_single_ref_account_offsets(&empty_offsets);
    assert_eq!(count4, 0, "Should handle empty slice");
    assert_eq!(storage.num_zero_lamport_single_ref_accounts(), 5);

    // Test inserting large batch with mixed duplicates
    let offsets5 = vec![10, 60, 20, 70, 30, 80, 40]; // 10,20,30,40 duplicates, 60,70,80 new
    let count5 = storage.batch_insert_zero_lamport_single_ref_account_offsets(&offsets5);
    assert_eq!(count5, 3, "Should insert only 3 new offsets (60, 70, 80)");
    assert_eq!(storage.num_zero_lamport_single_ref_accounts(), 8);
}
