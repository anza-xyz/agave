#[cfg(test)]
mod tests {
    use {
        crate::{
            bank::{test_utils as bank_test_utils, Bank},
            epoch_stakes::{EpochAuthorizedVoters, NodeIdToVoteAccounts, VersionedEpochStakes},
            genesis_utils::activate_all_features,
            runtime_config::RuntimeConfig,
            serde_snapshot::{self, ExtraFieldsToSerialize, SnapshotStreams},
            snapshot_bank_utils,
            snapshot_utils::{
                create_tmp_accounts_dir_for_tests, get_storages_to_serialize,
                StorageAndNextAccountsFileId,
            },
            stakes::{SerdeStakesToStakeFormat, Stakes},
        },
        agave_snapshots::snapshot_config::SnapshotConfig,
        solana_accounts_db::{
            account_storage::AccountStorageMap,
            accounts_db::{
                get_temp_accounts_paths, AccountStorageEntry, AccountsDb, AtomicAccountsFileId,
                ACCOUNTS_DB_CONFIG_FOR_TESTING,
            },
            accounts_file::{AccountsFile, AccountsFileError, StorageAccess},
            ObsoleteAccounts,
        },
        solana_epoch_schedule::EpochSchedule,
        solana_genesis_config::create_genesis_config,
        solana_pubkey::Pubkey,
        solana_stake_interface::state::Stake,
        std::{
            io::{BufReader, BufWriter, Cursor},
            mem,
            ops::RangeFull,
            path::Path,
            sync::{atomic::Ordering, Arc, OnceLock},
        },
        tempfile::TempDir,
        test_case::{test_case, test_matrix},
    };

    /// Simulates the unpacking & storage reconstruction done during snapshot unpacking
    fn copy_append_vecs<P: AsRef<Path>>(
        accounts_db: &AccountsDb,
        output_dir: P,
        storage_access: StorageAccess,
    ) -> Result<StorageAndNextAccountsFileId, AccountsFileError> {
        let storage_entries = accounts_db.get_storages(RangeFull).0;
        let storage: AccountStorageMap = AccountStorageMap::with_capacity(storage_entries.len());
        let mut next_append_vec_id = 0;
        for storage_entry in storage_entries.into_iter() {
            // Copy file to new directory
            let storage_path = storage_entry.path();
            let file_name = AccountsFile::file_name(storage_entry.slot(), storage_entry.id());
            let output_path = output_dir.as_ref().join(file_name);
            std::fs::copy(storage_path, &output_path)?;

            // Read new file into append-vec and build new entry
            let (accounts_file, _num_accounts) = AccountsFile::new_from_file(
                output_path,
                storage_entry.accounts.len(),
                storage_access,
            )?;
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

    /// Test roundtrip serialize/deserialize of a bank
    #[test_matrix(
        [#[allow(deprecated)] StorageAccess::Mmap, StorageAccess::File]
    )]
    fn test_serialize_bank_snapshot(storage_access: StorageAccess) {
        let (mut genesis_config, _) = create_genesis_config(500);
        genesis_config.epoch_schedule = EpochSchedule::custom(400, 400, false);
        let bank0 = Arc::new(Bank::new_for_tests(&genesis_config));
        let deposit_amount = bank0.get_minimum_balance_for_rent_exemption(0);
        let bank1 = Bank::new_from_parent(bank0.clone(), &Pubkey::default(), 1);

        // Create an account on a non-root fork
        let key1 = Pubkey::new_unique();
        bank_test_utils::deposit(&bank1, &key1, deposit_amount).unwrap();

        let bank2_slot = 2;
        let bank2 = Bank::new_from_parent(bank0, &Pubkey::default(), bank2_slot);

        // Test new account
        let key2 = Pubkey::new_unique();
        bank_test_utils::deposit(&bank2, &key2, deposit_amount).unwrap();
        assert_eq!(bank2.get_balance(&key2), deposit_amount);

        let key3 = Pubkey::new_unique();
        bank_test_utils::deposit(&bank2, &key3, 0).unwrap();

        let accounts_db = &bank2.rc.accounts.accounts_db;

        bank2.squash();
        bank2.force_flush_accounts_cache();

        let expected_accounts_lt_hash = bank2.accounts_lt_hash.lock().unwrap().clone();

        let mut buf = Vec::new();
        let cursor = Cursor::new(&mut buf);
        let mut writer = BufWriter::new(cursor);
        {
            let mut bank_fields = bank2.get_fields_to_serialize();
            let versioned_epoch_stakes = mem::take(&mut bank_fields.versioned_epoch_stakes);
            let accounts_lt_hash = Some(bank_fields.accounts_lt_hash.clone().into());
            serde_snapshot::serialize_bank_snapshot_into(
                &mut writer,
                bank_fields,
                bank2.get_bank_hash_stats(),
                &get_storages_to_serialize(&bank2.get_snapshot_storages(None)),
                ExtraFieldsToSerialize {
                    lamports_per_signature: bank2.fee_rate_governor.lamports_per_signature,
                    obsolete_incremental_snapshot_persistence: None,
                    obsolete_epoch_accounts_hash: None,
                    versioned_epoch_stakes,
                    accounts_lt_hash,
                },
                accounts_db.write_version.load(Ordering::Acquire),
            )
            .unwrap();
        }
        drop(writer);

        // Now deserialize the serialized bank and ensure it matches the original bank

        // Create a new set of directories for this bank's accounts
        let (_accounts_dir, dbank_paths) = get_temp_accounts_paths(4).unwrap();
        // Create a directory to simulate AppendVecs unpackaged from a snapshot tar
        let copied_accounts = TempDir::new().unwrap();
        let storage_and_next_append_vec_id =
            copy_append_vecs(accounts_db, copied_accounts.path(), storage_access).unwrap();

        let cursor = Cursor::new(buf.as_slice());
        let mut reader = BufReader::new(cursor);
        let mut snapshot_streams = SnapshotStreams {
            full_snapshot_stream: &mut reader,
            incremental_snapshot_stream: None,
        };
        let (dbank, _) = serde_snapshot::bank_from_streams(
            &mut snapshot_streams,
            &dbank_paths,
            storage_and_next_append_vec_id,
            &genesis_config,
            &RuntimeConfig::default(),
            None,
            None,
            false,
            ACCOUNTS_DB_CONFIG_FOR_TESTING,
            None,
            Arc::default(),
        )
        .unwrap();
        assert_eq!(dbank.get_balance(&key1), 0);
        assert_eq!(dbank.get_balance(&key2), deposit_amount);
        assert_eq!(dbank.get_balance(&key3), 0);
        assert_eq!(
            dbank.accounts_lt_hash.lock().unwrap().clone(),
            expected_accounts_lt_hash,
        );
        assert_eq!(dbank.get_bank_hash_stats(), bank2.get_bank_hash_stats());
        assert_eq!(dbank, bank2);
    }

    fn add_root_and_flush_write_cache(bank: &Bank) {
        bank.rc.accounts.add_root(bank.slot());
        bank.flush_accounts_cache_slot_for_tests()
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_extra_fields_eof(storage_access: StorageAccess) {
        agave_logger::setup();
        let (genesis_config, _) = create_genesis_config(500);

        let bank0 = Arc::new(Bank::new_for_tests(&genesis_config));
        bank0.squash();
        let mut bank = Bank::new_from_parent(bank0.clone(), &Pubkey::default(), 1);
        bank.freeze();
        add_root_and_flush_write_cache(&bank0);

        // Set extra fields
        bank.fee_rate_governor.lamports_per_signature = 7000;
        // Note that epoch_stakes already has two epoch stakes entries for epochs 0 and 1
        // which will also be serialized to the versioned epoch stakes extra field. Those
        // entries are of type Stakes<StakeAccount> so add a new entry for Stakes<Stake>.
        bank.epoch_stakes.insert(
            42,
            VersionedEpochStakes::Current {
                stakes: SerdeStakesToStakeFormat::Stake(Stakes::<Stake>::default()),
                total_stake: 42,
                node_id_to_vote_accounts: Arc::<NodeIdToVoteAccounts>::default(),
                epoch_authorized_voters: Arc::<EpochAuthorizedVoters>::default(),
                bls_pubkey_to_rank_map: OnceLock::new(),
            },
        );
        assert_eq!(bank.epoch_stakes.len(), 3);

        // Serialize
        let snapshot_storages = bank.get_snapshot_storages(None);
        let mut buf = vec![];
        let mut writer = Cursor::new(&mut buf);

        crate::serde_snapshot::bank_to_stream(
            &mut std::io::BufWriter::new(&mut writer),
            &bank,
            &get_storages_to_serialize(&snapshot_storages),
        )
        .unwrap();

        // Deserialize
        let rdr = Cursor::new(&buf[..]);
        let mut reader = std::io::BufReader::new(&buf[rdr.position() as usize..]);
        let mut snapshot_streams = SnapshotStreams {
            full_snapshot_stream: &mut reader,
            incremental_snapshot_stream: None,
        };
        let (_accounts_dir, dbank_paths) = get_temp_accounts_paths(4).unwrap();
        let copied_accounts = TempDir::new().unwrap();
        let storage_and_next_append_vec_id = copy_append_vecs(
            &bank.rc.accounts.accounts_db,
            copied_accounts.path(),
            storage_access,
        )
        .unwrap();
        let (dbank, _) = crate::serde_snapshot::bank_from_streams(
            &mut snapshot_streams,
            &dbank_paths,
            storage_and_next_append_vec_id,
            &genesis_config,
            &RuntimeConfig::default(),
            None,
            None,
            false,
            ACCOUNTS_DB_CONFIG_FOR_TESTING,
            None,
            Arc::default(),
        )
        .unwrap();

        assert_eq!(bank.epoch_stakes, dbank.epoch_stakes);
        assert_eq!(
            bank.fee_rate_governor.lamports_per_signature,
            dbank.fee_rate_governor.lamports_per_signature
        );
    }

    #[test]
    fn test_extra_fields_full_snapshot_archive() {
        agave_logger::setup();

        let (mut genesis_config, _) = create_genesis_config(500);
        activate_all_features(&mut genesis_config);

        let bank0 = Arc::new(Bank::new_for_tests(&genesis_config));
        let mut bank = Bank::new_from_parent(bank0, &Pubkey::default(), 1);
        while !bank.is_complete() {
            bank.fill_bank_with_ticks_for_tests();
        }

        // Set extra field
        bank.fee_rate_governor.lamports_per_signature = 7000;

        let (_tmp_dir, accounts_dir) = create_tmp_accounts_dir_for_tests();
        let bank_snapshots_dir = TempDir::new().unwrap();
        let full_snapshot_archives_dir = TempDir::new().unwrap();
        let incremental_snapshot_archives_dir = TempDir::new().unwrap();

        // Serialize
        let snapshot_archive_info = snapshot_bank_utils::bank_to_full_snapshot_archive(
            &bank_snapshots_dir,
            &bank,
            None,
            full_snapshot_archives_dir.path(),
            incremental_snapshot_archives_dir.path(),
            SnapshotConfig::default().archive_format,
        )
        .unwrap();

        // Deserialize
        let dbank = snapshot_bank_utils::bank_from_snapshot_archives(
            &[accounts_dir],
            bank_snapshots_dir.path(),
            &snapshot_archive_info,
            None,
            &genesis_config,
            &RuntimeConfig::default(),
            None,
            None,
            false,
            false,
            false,
            ACCOUNTS_DB_CONFIG_FOR_TESTING,
            None,
            Arc::default(),
        )
        .unwrap();

        assert_eq!(
            bank.fee_rate_governor.lamports_per_signature,
            dbank.fee_rate_governor.lamports_per_signature
        );
    }

    #[cfg(feature = "frozen-abi")]
    mod test_bank_serialize {
        use {
            super::*,
            crate::{bank::BankHashStats, serde_snapshot::ObsoleteIncrementalSnapshotPersistence},
            solana_accounts_db::accounts_hash::AccountsLtHash,
            solana_frozen_abi::abi_example::AbiExample,
            solana_hash::Hash,
            solana_lattice_hash::lt_hash::LtHash,
            std::marker::PhantomData,
        };

        // This some what long test harness is required to freeze the ABI of Bank's serialization,
        // which is implemented manually by calling serialize_bank_snapshot_with() mainly based on
        // get_fields_to_serialize(). However, note that Bank's serialization is coupled with
        // snapshot storages as well.
        //
        // It was avoided to impl AbiExample for Bank by wrapping it around PhantomData inside the
        // special wrapper called BankAbiTestWrapper. And internally, it creates an actual bank
        // from Bank::default_for_tests().
        //
        // In this way, frozen abi can increase the coverage of the serialization code path as much
        // as possible. Alternatively, we could derive AbiExample for the minimum set of actually
        // serialized fields of bank as an ad-hoc tuple. But that was avoided to avoid maintenance
        // burden instead.
        //
        // Involving the Bank here is preferred conceptually because snapshot abi is
        // important and snapshot is just a (rooted) serialized bank at the high level. Only
        // abi-freezing bank.get_fields_to_serialize() is kind of relying on the implementation
        // detail.
        #[cfg_attr(
            feature = "frozen-abi",
            derive(AbiExample),
            frozen_abi(digest = "HxmFy4D1VFmq91rp4PDAMsunMnwxqdeQ2CNyaNkStnEw")
        )]
        #[derive(serde::Serialize)]
        pub struct BankAbiTestWrapper {
            #[serde(serialize_with = "wrapper")]
            bank: PhantomData<Bank>,
        }

        pub fn wrapper<S>(_bank: &PhantomData<Bank>, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            let bank = Bank::default_for_tests();
            let snapshot_storages = AccountsDb::example().get_storages(0..1).0;
            // ensure there is at least one snapshot storage example for ABI digesting
            assert!(!snapshot_storages.is_empty());

            let incremental_snapshot_persistence = ObsoleteIncrementalSnapshotPersistence {
                full_slot: u64::default(),
                full_hash: [1; 32],
                full_capitalization: u64::default(),
                incremental_hash: [2; 32],
                incremental_capitalization: u64::default(),
            };

            let mut bank_fields = bank.get_fields_to_serialize();
            let versioned_epoch_stakes = std::mem::take(&mut bank_fields.versioned_epoch_stakes);
            serde_snapshot::serialize_bank_snapshot_with(
                serializer,
                bank_fields,
                BankHashStats::default(),
                &get_storages_to_serialize(&snapshot_storages),
                ExtraFieldsToSerialize {
                    lamports_per_signature: bank.fee_rate_governor.lamports_per_signature,
                    obsolete_incremental_snapshot_persistence: Some(
                        incremental_snapshot_persistence,
                    ),
                    obsolete_epoch_accounts_hash: Some(Hash::new_unique()),
                    versioned_epoch_stakes,
                    accounts_lt_hash: Some(AccountsLtHash(LtHash::identity()).into()),
                },
                u64::default(), // obsolete, formerly write_version
            )
        }
    }
}
