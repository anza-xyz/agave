#[cfg(test)]
mod serde_snapshot_tests {
    use {
        crate::{
            bank::BankHashStats,
            serde_snapshot::{
                deserialize_accounts_db_fields, reconstruct_accountsdb_from_fields,
                remap_append_vec_file, SerializableAccountsDb, SnapshotAccountsDbFields,
            },
            snapshot_utils::{get_storages_to_serialize, StorageAndNextAccountsFileId},
        },
        bincode::{serialize_into, Error},
        log::info,
        rand::{rng, Rng},
        solana_account::{AccountSharedData, ReadableAccount},
        solana_accounts_db::{
            account_storage::AccountStorageMap,
            account_storage_reader::AccountStorageReader,
            accounts::Accounts,
            accounts_db::{
                get_temp_accounts_paths, AccountStorageEntry, AccountsDb, AccountsDbConfig,
                AtomicAccountsFileId, MarkObsoleteAccounts, ACCOUNTS_DB_CONFIG_FOR_TESTING,
            },
            accounts_file::{AccountsFile, AccountsFileError, StorageAccess},
            ancestors::Ancestors,
            ObsoleteAccounts,
        },
        solana_clock::Slot,
        solana_epoch_schedule::EpochSchedule,
        solana_pubkey::Pubkey,
        std::{
            fs::File,
            io::{self, BufReader, Cursor, Read, Write},
            ops::RangeFull,
            path::{Path, PathBuf},
            sync::{
                atomic::{AtomicUsize, Ordering},
                Arc,
            },
        },
        tempfile::TempDir,
        test_case::{test_case, test_matrix},
    };

    fn linear_ancestors(end_slot: u64) -> Ancestors {
        let mut ancestors: Ancestors = vec![(0, 0)].into_iter().collect();
        for i in 1..end_slot {
            ancestors.insert(i, (i - 1) as usize);
        }
        ancestors
    }

    fn context_accountsdb_from_stream<R>(
        stream: &mut BufReader<R>,
        account_paths: &[PathBuf],
        storage_and_next_append_vec_id: StorageAndNextAccountsFileId,
        accounts_db_config: AccountsDbConfig,
    ) -> Result<AccountsDb, Error>
    where
        R: Read,
    {
        // read and deserialise the accounts database directly from the stream
        let accounts_db_fields = deserialize_accounts_db_fields(stream)?;
        let snapshot_accounts_db_fields = SnapshotAccountsDbFields {
            full_snapshot_accounts_db_fields: accounts_db_fields,
            incremental_snapshot_accounts_db_fields: None,
        };
        reconstruct_accountsdb_from_fields(
            snapshot_accounts_db_fields,
            account_paths,
            storage_and_next_append_vec_id,
            None,
            false,
            accounts_db_config,
            None,
            Arc::default(),
        )
        .map(|(accounts_db, _)| accounts_db)
    }

    fn accountsdb_from_stream<R>(
        stream: &mut BufReader<R>,
        account_paths: &[PathBuf],
        storage_and_next_append_vec_id: StorageAndNextAccountsFileId,
        accounts_db_config: AccountsDbConfig,
    ) -> Result<AccountsDb, Error>
    where
        R: Read,
    {
        context_accountsdb_from_stream::<R>(
            stream,
            account_paths,
            storage_and_next_append_vec_id,
            accounts_db_config,
        )
    }

    fn accountsdb_to_stream<W>(
        stream: &mut W,
        accounts_db: &AccountsDb,
        slot: Slot,
        account_storage_entries: &[Vec<Arc<AccountStorageEntry>>],
    ) -> Result<(), Error>
    where
        W: Write,
    {
        let bank_hash_stats = BankHashStats::default();
        let write_version = accounts_db.write_version.load(Ordering::Acquire);
        serialize_into(
            stream,
            &SerializableAccountsDb {
                slot,
                account_storage_entries,
                bank_hash_stats,
                write_version,
            },
        )
    }

    /// Simulates the unpacking & storage reconstruction done during snapshot unpacking
    fn copy_append_vecs(
        accounts_db: &AccountsDb,
        output_dir: impl AsRef<Path>,
        storage_access: StorageAccess,
    ) -> Result<StorageAndNextAccountsFileId, AccountsFileError> {
        let storage_entries = accounts_db.get_storages(RangeFull).0;
        let storage: AccountStorageMap = AccountStorageMap::with_capacity(storage_entries.len());
        let mut next_append_vec_id = 0;
        for storage_entry in storage_entries.into_iter() {
            // Copy file to new directory
            let file_name = AccountsFile::file_name(storage_entry.slot(), storage_entry.id());
            let output_path = output_dir.as_ref().join(file_name);
            let mut reader = AccountStorageReader::new(&storage_entry, None).unwrap();
            let mut writer = File::create(&output_path)?;
            io::copy(&mut reader, &mut writer)?;

            // Read new file into append-vec and build new entry
            let (accounts_file, _num_accounts) =
                AccountsFile::new_from_file(output_path, reader.len(), storage_access)?;
            let new_storage_entry = AccountStorageEntry::new_existing(
                storage_entry.slot(),
                storage_entry.id(),
                accounts_file,
                ObsoleteAccounts::default(),
            );
            next_append_vec_id = next_append_vec_id.max(new_storage_entry.id());
            storage.insert(new_storage_entry.slot(), Arc::new(new_storage_entry));
        }

        Ok(StorageAndNextAccountsFileId {
            storage,
            next_append_vec_id: AtomicAccountsFileId::new(next_append_vec_id + 1),
        })
    }

    fn reconstruct_accounts_db_via_serialization(
        accounts: &AccountsDb,
        slot: Slot,
        storage_access: StorageAccess,
        accounts_db_config: AccountsDbConfig,
    ) -> AccountsDb {
        let mut writer = Cursor::new(vec![]);
        let snapshot_storages = accounts.get_storages(..=slot).0;
        accountsdb_to_stream(
            &mut writer,
            accounts,
            slot,
            &get_storages_to_serialize(&snapshot_storages),
        )
        .unwrap();

        let buf = writer.into_inner();
        let mut reader = BufReader::new(&buf[..]);
        let copied_accounts = TempDir::new().unwrap();

        // Simulate obtaining a copy of the AppendVecs from a tarball
        let storage_and_next_append_vec_id =
            copy_append_vecs(accounts, copied_accounts.path(), storage_access).unwrap();
        let mut accounts_db = accountsdb_from_stream(
            &mut reader,
            &[],
            storage_and_next_append_vec_id,
            accounts_db_config,
        )
        .unwrap();

        // The append vecs will be used from `copied_accounts` directly by the new AccountsDb so keep
        // its TempDir alive
        accounts_db
            .temp_paths
            .as_mut()
            .unwrap()
            .push(copied_accounts);

        accounts_db
    }

    fn check_accounts_local(accounts: &Accounts, pubkeys: &[Pubkey], num: usize) {
        for _ in 1..num {
            let idx = rng().random_range(0..num - 1);
            let ancestors = vec![(0, 0)].into_iter().collect();
            let account = accounts.load_without_fixed_root(&ancestors, &pubkeys[idx]);
            let account1 = Some((
                AccountSharedData::new((idx + 1) as u64, 0, AccountSharedData::default().owner()),
                0,
            ));
            assert_eq!(account, account1);
        }
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    fn test_accounts_serialize(storage_access: StorageAccess) {
        agave_logger::setup();
        let (_accounts_dir, paths) = get_temp_accounts_paths(4).unwrap();
        let accounts_db = AccountsDb::new_for_tests(paths);
        let accounts = Accounts::new(Arc::new(accounts_db));

        let slot = 0;
        let pubkeys: Vec<_> = std::iter::repeat_with(solana_pubkey::new_rand)
            .take(100)
            .collect();
        for (i, pubkey) in pubkeys.iter().enumerate() {
            let account = AccountSharedData::new(i as u64 + 1, 0, &Pubkey::default());
            accounts.store_accounts_seq((slot, [(pubkey, &account)].as_slice()), None);
        }
        check_accounts_local(&accounts, &pubkeys, 100);
        accounts.accounts_db.add_root_and_flush_write_cache(slot);
        let accounts_hash = accounts
            .accounts_db
            .calculate_accounts_lt_hash_at_startup_from_index(&Ancestors::default(), slot);

        let mut writer = Cursor::new(vec![]);
        accountsdb_to_stream(
            &mut writer,
            &accounts.accounts_db,
            slot,
            &get_storages_to_serialize(&accounts.accounts_db.get_storages(..=slot).0),
        )
        .unwrap();

        let copied_accounts = TempDir::new().unwrap();

        // Simulate obtaining a copy of the AppendVecs from a tarball
        let storage_and_next_append_vec_id = copy_append_vecs(
            &accounts.accounts_db,
            copied_accounts.path(),
            storage_access,
        )
        .unwrap();

        let buf = writer.into_inner();
        let mut reader = BufReader::new(&buf[..]);
        let (_accounts_dir, daccounts_paths) = get_temp_accounts_paths(2).unwrap();
        let daccounts = Accounts::new(Arc::new(
            accountsdb_from_stream(
                &mut reader,
                &daccounts_paths,
                storage_and_next_append_vec_id,
                ACCOUNTS_DB_CONFIG_FOR_TESTING,
            )
            .unwrap(),
        ));
        check_accounts_local(&daccounts, &pubkeys, 100);
        let daccounts_hash = accounts
            .accounts_db
            .calculate_accounts_lt_hash_at_startup_from_index(&Ancestors::default(), slot);
        assert_eq!(accounts_hash, daccounts_hash);
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_remove_unrooted_slot_snapshot(storage_access: StorageAccess) {
        agave_logger::setup();
        let unrooted_slot = 9;
        let unrooted_bank_id = 9;
        let db = AccountsDb::new_single_for_tests();
        let key = solana_pubkey::new_rand();
        let account0 = AccountSharedData::new(1, 0, &key);
        db.store_for_tests((unrooted_slot, [(&key, &account0)].as_slice()));

        // Purge the slot
        db.remove_unrooted_slots(&[(unrooted_slot, unrooted_bank_id)]);

        // Add a new root
        let key2 = solana_pubkey::new_rand();
        let new_root = unrooted_slot + 1;
        db.store_for_tests((new_root, [(&key2, &account0)].as_slice()));
        db.add_root_and_flush_write_cache(new_root);

        // Simulate reconstruction from snapshot
        let db = reconstruct_accounts_db_via_serialization(
            &db,
            new_root,
            storage_access,
            ACCOUNTS_DB_CONFIG_FOR_TESTING,
        );

        // Check root account exists
        db.assert_load_account(new_root, key2, 1);

        // Check purged account stays gone
        let unrooted_slot_ancestors = vec![(unrooted_slot, 1)].into_iter().collect();
        assert!(db
            .load_without_fixed_root(&unrooted_slot_ancestors, &key)
            .is_none());
    }

    #[test_matrix(
        [StorageAccess::File, #[allow(deprecated)] StorageAccess::Mmap],
        [MarkObsoleteAccounts::Enabled, MarkObsoleteAccounts::Disabled],
        [MarkObsoleteAccounts::Enabled, MarkObsoleteAccounts::Disabled]
    )]
    fn test_accounts_db_serialize1(
        storage_access: StorageAccess,
        mark_obsolete_accounts_initial: MarkObsoleteAccounts,
        mark_obsolete_accounts_restore: MarkObsoleteAccounts,
    ) {
        for pass in 0..2 {
            agave_logger::setup();
            let accounts = AccountsDb::new_with_config(
                Vec::new(),
                AccountsDbConfig {
                    mark_obsolete_accounts: mark_obsolete_accounts_initial,
                    ..ACCOUNTS_DB_CONFIG_FOR_TESTING
                },
                None,
                Arc::default(),
            );
            let mut pubkeys: Vec<Pubkey> = vec![];

            // Create 100 accounts in slot 0
            accounts.create_account(&mut pubkeys, 0, 100, 0, 0);
            if pass == 0 {
                accounts.add_root_and_flush_write_cache(0);
                accounts.check_storage(0, 100, 100);
                accounts.clean_accounts_for_tests();
                accounts.check_accounts(&pubkeys, 0, 100, 1);
                // clean should have done nothing
                continue;
            }

            // do some updates to those accounts and re-check
            accounts.modify_accounts(&pubkeys, 0, 100, 2);
            accounts.add_root_and_flush_write_cache(0);
            accounts.check_storage(0, 100, 100);
            accounts.check_accounts(&pubkeys, 0, 100, 2);

            let mut pubkeys1: Vec<Pubkey> = vec![];

            // CREATE SLOT 1
            let latest_slot = 1;

            // Modify the first 10 of the accounts from slot 0 in slot 1
            accounts.modify_accounts(&pubkeys, latest_slot, 10, 3);
            // Overwrite account 30 from slot 0 with lamports=0 into slot 1.
            // Slot 1 should now have 10 + 1 = 11 accounts
            let account = AccountSharedData::new(0, 0, AccountSharedData::default().owner());
            accounts.store_for_tests((latest_slot, [(&pubkeys[30], &account)].as_slice()));

            // Create 10 new accounts in slot 1, should now have 11 + 10 = 21
            // accounts
            accounts.create_account(&mut pubkeys1, latest_slot, 10, 0, 0);

            accounts.add_root_and_flush_write_cache(latest_slot);
            accounts.check_storage(1, 21, 21);

            // CREATE SLOT 2
            let latest_slot = 2;
            let mut pubkeys2: Vec<Pubkey> = vec![];

            // Modify first 20 of the accounts from slot 0 in slot 2
            accounts.modify_accounts(&pubkeys, latest_slot, 20, 4);
            accounts.clean_accounts_for_tests();
            // Overwrite account 31 from slot 0 with lamports=0 into slot 2.
            // Slot 2 should now have 20 + 1 = 21 accounts
            let account = AccountSharedData::new(0, 0, AccountSharedData::default().owner());
            accounts.store_for_tests((latest_slot, [(&pubkeys[31], &account)].as_slice()));

            // Create 10 new accounts in slot 2. Slot 2 should now have
            // 21 + 10 = 31 accounts
            accounts.create_account(&mut pubkeys2, latest_slot, 10, 0, 0);

            accounts.add_root_and_flush_write_cache(latest_slot);
            accounts.check_storage(2, 31, 31);

            let ancestors = linear_ancestors(latest_slot);

            accounts.clean_accounts_for_tests();
            // The first 20 accounts of slot 0 have been updated in slot 2, as well as
            // accounts 30 and  31 (overwritten with zero-lamport accounts in slot 1 and
            // slot 2 respectively), so only 78 accounts are left in slot 0's storage entries.
            accounts.check_storage(0, 78, 100);
            // 10 of the 21 accounts have been modified in slot 2, so only 11
            // accounts left in slot 1.
            accounts.check_storage(1, 11, 21);
            accounts.check_storage(2, 31, 31);

            let accounts_db_config = AccountsDbConfig {
                mark_obsolete_accounts: mark_obsolete_accounts_restore,
                ..ACCOUNTS_DB_CONFIG_FOR_TESTING
            };
            let daccounts = reconstruct_accounts_db_via_serialization(
                &accounts,
                latest_slot,
                storage_access,
                accounts_db_config,
            );

            assert_eq!(
                daccounts.write_version.load(Ordering::Acquire),
                accounts.write_version.load(Ordering::Acquire)
            );

            daccounts.print_count_and_status("daccounts");

            // Don't check the first 35 accounts which have not been modified on slot 0
            daccounts.check_accounts(&pubkeys[35..], 0, 65, 37);
            daccounts.check_accounts(&pubkeys1, 1, 10, 1);

            // If accounts are marked obsolete at initial save time, then the accounts will be
            // shrunk during snapshot archive
            if mark_obsolete_accounts_initial == MarkObsoleteAccounts::Enabled {
                daccounts.check_storage(0, 78, 78);
                daccounts.check_storage(1, 11, 11);
            // If accounts are marked obsolete at restore time, then the accounts will be marked
            // obsolete and cleaned during snapshot restore but not removed from the storages until
            // the next shrink
            } else if mark_obsolete_accounts_restore == MarkObsoleteAccounts::Enabled {
                daccounts.check_storage(0, 78, 100);
                daccounts.check_storage(1, 11, 21);
            } else {
                daccounts.check_storage(0, 100, 100);
                daccounts.check_storage(1, 21, 21);
            }

            daccounts.check_storage(2, 31, 31);

            assert_eq!(
                daccounts.calculate_accounts_lt_hash_at_startup_from_index(&ancestors, latest_slot),
                accounts.calculate_accounts_lt_hash_at_startup_from_index(&ancestors, latest_slot),
            );
        }
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_accounts_db_serialize_zero_and_free(storage_access: StorageAccess) {
        agave_logger::setup();

        let some_lamport = 223;
        let zero_lamport = 0;
        let no_data = 0;
        let owner = *AccountSharedData::default().owner();

        let account = AccountSharedData::new(some_lamport, no_data, &owner);
        let pubkey = solana_pubkey::new_rand();
        let zero_lamport_account = AccountSharedData::new(zero_lamport, no_data, &owner);

        let account2 = AccountSharedData::new(some_lamport + 1, no_data, &owner);
        let pubkey2 = solana_pubkey::new_rand();

        let accounts = AccountsDb::new_single_for_tests();

        let mut current_slot = 1;
        accounts.store_for_tests((current_slot, [(&pubkey, &account)].as_slice()));
        accounts.add_root(current_slot);

        current_slot += 1;
        accounts.store_for_tests((current_slot, [(&pubkey, &zero_lamport_account)].as_slice()));
        accounts.store_for_tests((current_slot, [(&pubkey2, &account2)].as_slice()));

        accounts.add_root_and_flush_write_cache(current_slot);

        accounts.assert_load_account(current_slot, pubkey, zero_lamport);

        accounts.print_accounts_stats("accounts");

        accounts.clean_accounts_for_tests();

        accounts.print_accounts_stats("accounts_post_purge");

        let accounts = reconstruct_accounts_db_via_serialization(
            &accounts,
            current_slot,
            storage_access,
            ACCOUNTS_DB_CONFIG_FOR_TESTING,
        );

        accounts.print_accounts_stats("reconstructed");

        accounts.assert_load_account(current_slot, pubkey, zero_lamport);
    }

    fn with_chained_zero_lamport_accounts<F>(f: F)
    where
        F: Fn(AccountsDb, Slot) -> AccountsDb,
    {
        let some_lamport = 223;
        let zero_lamport = 0;
        let dummy_lamport = 999;
        let no_data = 0;
        let owner = *AccountSharedData::default().owner();

        let account = AccountSharedData::new(some_lamport, no_data, &owner);
        let account2 = AccountSharedData::new(some_lamport + 100_001, no_data, &owner);
        let account3 = AccountSharedData::new(some_lamport + 100_002, no_data, &owner);
        let zero_lamport_account = AccountSharedData::new(zero_lamport, no_data, &owner);

        let pubkey = solana_pubkey::new_rand();
        let purged_pubkey1 = solana_pubkey::new_rand();
        let purged_pubkey2 = solana_pubkey::new_rand();

        let dummy_account = AccountSharedData::new(dummy_lamport, no_data, &owner);
        let dummy_pubkey = Pubkey::default();

        let accounts = AccountsDb::new_single_for_tests();

        let mut current_slot = 1;
        accounts.store_for_tests((current_slot, [(&pubkey, &account)].as_slice()));
        accounts.store_for_tests((current_slot, [(&purged_pubkey1, &account2)].as_slice()));
        accounts.add_root_and_flush_write_cache(current_slot);

        current_slot += 1;
        accounts.store_for_tests((
            current_slot,
            [(&purged_pubkey1, &zero_lamport_account)].as_slice(),
        ));
        accounts.store_for_tests((current_slot, [(&purged_pubkey2, &account3)].as_slice()));
        accounts.add_root_and_flush_write_cache(current_slot);

        current_slot += 1;
        accounts.store_for_tests((
            current_slot,
            [(&purged_pubkey2, &zero_lamport_account)].as_slice(),
        ));
        accounts.add_root_and_flush_write_cache(current_slot);

        current_slot += 1;
        accounts.store_for_tests((current_slot, [(&dummy_pubkey, &dummy_account)].as_slice()));
        accounts.add_root_and_flush_write_cache(current_slot);

        accounts.print_accounts_stats("pre_f");

        let accounts = f(accounts, current_slot);

        accounts.print_accounts_stats("post_f");

        accounts.assert_load_account(current_slot, pubkey, some_lamport);
        accounts.assert_load_account(current_slot, purged_pubkey1, 0);
        accounts.assert_load_account(current_slot, purged_pubkey2, 0);
        accounts.assert_load_account(current_slot, dummy_pubkey, dummy_lamport);

        let calculated_capitalization =
            accounts.calculate_capitalization_at_startup_from_index(&Ancestors::default(), 4);
        let expected_capitalization = 1_222;
        assert_eq!(calculated_capitalization, expected_capitalization);
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_accounts_purge_chained_purge_before_snapshot_restore(storage_access: StorageAccess) {
        agave_logger::setup();
        with_chained_zero_lamport_accounts(|accounts, current_slot| {
            // If there is no latest full snapshot, zero lamport accounts can be cleaned and
            // removed immediately. Set latest full snapshot slot to zero to avoid cleaning
            // zero lamport accounts
            accounts.set_latest_full_snapshot_slot(0);
            accounts.clean_accounts_for_tests();
            reconstruct_accounts_db_via_serialization(
                &accounts,
                current_slot,
                storage_access,
                ACCOUNTS_DB_CONFIG_FOR_TESTING,
            )
        });
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_accounts_purge_chained_purge_after_snapshot_restore(storage_access: StorageAccess) {
        agave_logger::setup();
        with_chained_zero_lamport_accounts(|accounts, current_slot| {
            let accounts = reconstruct_accounts_db_via_serialization(
                &accounts,
                current_slot,
                storage_access,
                ACCOUNTS_DB_CONFIG_FOR_TESTING,
            );
            accounts.print_accounts_stats("after_reconstruct");
            accounts.set_latest_full_snapshot_slot(0);
            accounts.clean_accounts_for_tests();
            reconstruct_accounts_db_via_serialization(
                &accounts,
                current_slot,
                storage_access,
                ACCOUNTS_DB_CONFIG_FOR_TESTING,
            )
        });
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_accounts_purge_long_chained_after_snapshot_restore(storage_access: StorageAccess) {
        agave_logger::setup();
        let old_lamport = 223;
        let zero_lamport = 0;
        let no_data = 0;
        let owner = *AccountSharedData::default().owner();

        let account = AccountSharedData::new(old_lamport, no_data, &owner);
        let account2 = AccountSharedData::new(old_lamport + 100_001, no_data, &owner);
        let account3 = AccountSharedData::new(old_lamport + 100_002, no_data, &owner);
        let dummy_account = AccountSharedData::new(99_999_999, no_data, &owner);
        let zero_lamport_account = AccountSharedData::new(zero_lamport, no_data, &owner);

        let pubkey = solana_pubkey::new_rand();
        let dummy_pubkey = solana_pubkey::new_rand();
        let purged_pubkey1 = solana_pubkey::new_rand();
        let purged_pubkey2 = solana_pubkey::new_rand();

        let mut current_slot = 0;
        let accounts = AccountsDb::new_single_for_tests();

        // create intermediate updates to purged_pubkey1 so that
        // generate_index must add slots as root last at once
        current_slot += 1;
        accounts.store_for_tests((current_slot, [(&pubkey, &account)].as_slice()));
        accounts.store_for_tests((current_slot, [(&purged_pubkey1, &account2)].as_slice()));
        accounts.add_root_and_flush_write_cache(current_slot);

        current_slot += 1;
        accounts.store_for_tests((current_slot, [(&purged_pubkey1, &account2)].as_slice()));
        accounts.add_root_and_flush_write_cache(current_slot);

        current_slot += 1;
        accounts.store_for_tests((current_slot, [(&purged_pubkey1, &account2)].as_slice()));
        accounts.add_root_and_flush_write_cache(current_slot);

        current_slot += 1;
        accounts.store_for_tests((
            current_slot,
            [(&purged_pubkey1, &zero_lamport_account)].as_slice(),
        ));
        accounts.store_for_tests((current_slot, [(&purged_pubkey2, &account3)].as_slice()));
        accounts.add_root_and_flush_write_cache(current_slot);

        current_slot += 1;
        accounts.store_for_tests((
            current_slot,
            [(&purged_pubkey2, &zero_lamport_account)].as_slice(),
        ));
        accounts.add_root_and_flush_write_cache(current_slot);

        current_slot += 1;
        accounts.store_for_tests((current_slot, [(&dummy_pubkey, &dummy_account)].as_slice()));
        accounts.add_root_and_flush_write_cache(current_slot);

        accounts.print_count_and_status("before reconstruct");
        let accounts = reconstruct_accounts_db_via_serialization(
            &accounts,
            current_slot,
            storage_access,
            ACCOUNTS_DB_CONFIG_FOR_TESTING,
        );
        accounts.set_latest_full_snapshot_slot(0);
        accounts.print_count_and_status("before purge zero");
        accounts.clean_accounts_for_tests();
        accounts.print_count_and_status("after purge zero");

        accounts.assert_load_account(current_slot, pubkey, old_lamport);
        accounts.assert_load_account(current_slot, purged_pubkey1, 0);
        accounts.assert_load_account(current_slot, purged_pubkey2, 0);
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_accounts_clean_after_snapshot_restore_then_old_revives(storage_access: StorageAccess) {
        agave_logger::setup();
        let old_lamport = 223;
        let zero_lamport = 0;
        let no_data = 0;
        let dummy_lamport = 999_999;
        let owner = *AccountSharedData::default().owner();

        let account = AccountSharedData::new(old_lamport, no_data, &owner);
        let account2 = AccountSharedData::new(old_lamport + 100_001, no_data, &owner);
        let account3 = AccountSharedData::new(old_lamport + 100_002, no_data, &owner);
        let dummy_account = AccountSharedData::new(dummy_lamport, no_data, &owner);
        let zero_lamport_account = AccountSharedData::new(zero_lamport, no_data, &owner);

        let pubkey1 = solana_pubkey::new_rand();
        let pubkey2 = solana_pubkey::new_rand();
        let dummy_pubkey = solana_pubkey::new_rand();

        let mut current_slot = 0;
        let accounts = AccountsDb::new_single_for_tests();

        accounts.set_latest_full_snapshot_slot(0);

        // A: Initialize AccountsDb with pubkey1 and pubkey2
        current_slot += 1;
        accounts.store_for_tests((current_slot, [(&pubkey1, &account)].as_slice()));
        accounts.store_for_tests((current_slot, [(&pubkey2, &account)].as_slice()));
        accounts.add_root(current_slot);

        // B: Test multiple updates to pubkey1 in a single slot/storage
        current_slot += 1;
        assert_eq!(0, accounts.alive_account_count_in_slot(current_slot));
        accounts.add_root_and_flush_write_cache(current_slot - 1);
        accounts.assert_ref_count(&pubkey1, 1);
        accounts.store_for_tests((current_slot, [(&pubkey1, &account2)].as_slice()));
        accounts.store_for_tests((current_slot, [(&pubkey1, &account2)].as_slice()));
        accounts.add_root_and_flush_write_cache(current_slot);
        assert_eq!(1, accounts.alive_account_count_in_slot(current_slot));
        // Stores to same pubkey, same slot only count once towards the
        accounts.assert_ref_count(&pubkey1, 2);

        // C: Yet more update to trigger lazy clean of step A
        current_slot += 1;
        accounts.assert_ref_count(&pubkey1, 2);
        accounts.store_for_tests((current_slot, [(&pubkey1, &account3)].as_slice()));
        accounts.add_root_and_flush_write_cache(current_slot);
        accounts.assert_ref_count(&pubkey1, 3);
        accounts.add_root_and_flush_write_cache(current_slot);

        // D: Make pubkey1 0-lamport; also triggers clean of step B
        current_slot += 1;
        accounts.assert_ref_count(&pubkey1, 3);
        accounts.store_for_tests((current_slot, [(&pubkey1, &zero_lamport_account)].as_slice()));
        accounts.add_root_and_flush_write_cache(current_slot);
        // had to be a root to flush, but clean won't work as this test expects if it is a root
        // so, remove the root from alive_roots, then restore it after clean
        accounts
            .accounts_index
            .roots_tracker
            .write()
            .unwrap()
            .alive_roots
            .remove(&current_slot);
        accounts.clean_accounts_for_tests();
        accounts
            .accounts_index
            .roots_tracker
            .write()
            .unwrap()
            .alive_roots
            .insert(current_slot);

        // Removed one reference from the dead slot (reference only counted once
        // even though there were two stores to the pubkey in that slot)
        accounts.assert_ref_count(&pubkey1, 3);
        accounts.add_root(current_slot);

        // E: Avoid missing bank hash error
        current_slot += 1;
        accounts.store_for_tests((current_slot, [(&dummy_pubkey, &dummy_account)].as_slice()));
        accounts.add_root(current_slot);

        accounts.assert_load_account(current_slot, pubkey1, zero_lamport);
        accounts.assert_load_account(current_slot, pubkey2, old_lamport);
        accounts.assert_load_account(current_slot, dummy_pubkey, dummy_lamport);

        // At this point, there is no index entries for A and B
        // If step C and step D should be purged, snapshot restore would cause
        // pubkey1 to be revived as the state of step A.
        // So, prevent that from happening by introducing refcount
        ((current_slot - 1)..=current_slot).for_each(|slot| accounts.flush_root_write_cache(slot));
        accounts.clean_accounts_for_tests();
        let accounts = reconstruct_accounts_db_via_serialization(
            &accounts,
            current_slot,
            storage_access,
            ACCOUNTS_DB_CONFIG_FOR_TESTING,
        );

        // Set snapshot to zero to avoid cleaning zero-lamport pubkey1
        accounts.set_latest_full_snapshot_slot(0);
        accounts.clean_accounts_for_tests();

        info!("pubkey: {pubkey1}");
        accounts.print_accounts_stats("pre_clean");
        accounts.assert_load_account(current_slot, pubkey1, zero_lamport);
        accounts.assert_load_account(current_slot, pubkey2, old_lamport);
        accounts.assert_load_account(current_slot, dummy_pubkey, dummy_lamport);

        // F: Finally, make Step A cleanable
        current_slot += 1;
        accounts.store_for_tests((current_slot, [(&pubkey2, &account)].as_slice()));
        accounts.add_root(current_slot);

        // Do clean
        accounts.flush_root_write_cache(current_slot);

        // Make zero-lamport pubkey1 cleanable by setting the latest snapshot slot
        accounts.set_latest_full_snapshot_slot(current_slot);
        accounts.clean_accounts_for_tests();

        // 2nd clean needed to clean-up pubkey1
        accounts.clean_accounts_for_tests();

        // Ensure pubkey2 is cleaned from the index finally
        accounts.assert_not_load_account(current_slot, pubkey1);
        accounts.assert_load_account(current_slot, pubkey2, old_lamport);
        accounts.assert_load_account(current_slot, dummy_pubkey, dummy_lamport);
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_shrink_stale_slots_processed(storage_access: StorageAccess) {
        agave_logger::setup();

        for startup in &[false, true] {
            let accounts = AccountsDb::new_single_for_tests();

            let pubkey_count = 100;
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
            let pubkey_count_after_shrink = 10;
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
            accounts.shrink_all_slots(*startup, &EpochSchedule::default(), None);
            assert_eq!(
                pubkey_count_after_shrink,
                accounts.all_account_count_in_accounts_file(shrink_slot)
            );

            let no_ancestors = Ancestors::default();
            let epoch_schedule = EpochSchedule::default();

            let calculated_capitalization = accounts
                .calculate_capitalization_at_startup_from_index(&no_ancestors, current_slot);
            let expected_capitalization = 22_300;
            assert_eq!(calculated_capitalization, expected_capitalization);

            let accounts_lt_hash_pre = accounts
                .calculate_accounts_lt_hash_at_startup_from_index(&no_ancestors, current_slot);
            let accounts = reconstruct_accounts_db_via_serialization(
                &accounts,
                current_slot,
                storage_access,
                ACCOUNTS_DB_CONFIG_FOR_TESTING,
            );
            let accounts_lt_hash_post = accounts
                .calculate_accounts_lt_hash_at_startup_from_index(&no_ancestors, current_slot);
            assert_eq!(accounts_lt_hash_pre, accounts_lt_hash_post);

            // repeating should be no-op
            accounts.shrink_all_slots(*startup, &epoch_schedule, None);
            assert_eq!(
                pubkey_count_after_shrink,
                accounts.all_account_count_in_accounts_file(shrink_slot)
            );
        }
    }

    // no remap needed
    #[test_case(456, 456, 456, 0, |_| {})]
    // remap from 456 to 457, no collisions
    #[test_case(456, 457, 457, 0, |_| {})]
    // attempt to remap from 456 to 457, but there's a collision, so we get 458
    #[test_case(456, 457, 458, 1, |tmp| {
        File::create(tmp.join("123.457")).unwrap();
    })]
    fn test_remap_append_vec_file(
        old_id: usize,
        next_id: usize,
        expected_remapped_id: usize,
        expected_collisions: usize,
        become_ungovernable: impl FnOnce(&Path),
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let old_path = tmp.path().join(format!("123.{old_id}"));
        let expected_remapped_path = tmp.path().join(format!("123.{expected_remapped_id}"));
        File::create(&old_path).unwrap();

        become_ungovernable(tmp.path());

        let next_append_vec_id = AtomicAccountsFileId::new(next_id as u32);
        let num_collisions = AtomicUsize::new(0);
        let (remapped_id, remapped_path) =
            remap_append_vec_file(123, old_id, &old_path, &next_append_vec_id, &num_collisions)
                .unwrap();
        assert_eq!(remapped_id as usize, expected_remapped_id);
        assert_eq!(&remapped_path, &expected_remapped_path);
        assert_eq!(num_collisions.load(Ordering::Relaxed), expected_collisions);
    }

    #[test]
    #[should_panic(expected = "No such file or directory")]
    fn test_remap_append_vec_file_error() {
        let tmp = tempfile::tempdir().unwrap();
        let original_path = tmp.path().join("123.456");

        // In remap_append_vec_file() we want to handle EEXIST (collisions), but we want to return all
        // other errors
        let next_append_vec_id = AtomicAccountsFileId::new(457);
        let num_collisions = AtomicUsize::new(0);
        remap_append_vec_file(
            123,
            456,
            &original_path,
            &next_append_vec_id,
            &num_collisions,
        )
        .unwrap();
    }
}
