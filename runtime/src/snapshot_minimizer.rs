//! Used to create minimal snapshots - separated here to keep accounts_db simpler
#![cfg(feature = "dev-context-only-utils")]

use {
    crate::{bank::Bank, static_ids},
    agave_reserved_account_keys::ReservedAccountKeys,
    dashmap::DashSet,
    log::info,
    rayon::{
        iter::{IntoParallelIterator, IntoParallelRefIterator, ParallelIterator},
        prelude::ParallelSlice,
    },
    solana_account::{state_traits::StateMut, ReadableAccount},
    solana_accounts_db::{
        accounts_db::{
            stats::PurgeStats, AccountStorageEntry, AccountsDb, GetUniqueAccountsResult,
            UpdateIndexThreadSelection,
        },
        storable_accounts::StorableAccountsBySlot,
    },
    solana_clock::Slot,
    solana_loader_v3_interface::state::UpgradeableLoaderState,
    solana_measure::measure_time,
    solana_pubkey::Pubkey,
    solana_sdk_ids::bpf_loader_upgradeable,
    std::{
        collections::HashSet,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
    },
};

/// Used to modify bank and accounts_db to create a minimized snapshot
pub struct SnapshotMinimizer<'a> {
    bank: &'a Bank,
    starting_slot: Slot,
    minimized_account_set: DashSet<Pubkey>,
}

impl<'a> SnapshotMinimizer<'a> {
    /// Removes all accounts not necessary for replaying slots in the range [starting_slot, ending_slot].
    /// `transaction_account_set` should contain accounts used in transactions in the slot range [starting_slot, ending_slot].
    /// This function will accumulate other accounts (builtins, etc) necessary to replay transactions.
    ///
    /// This function will modify accounts_db by removing accounts not needed to replay [starting_slot, ending_slot],
    /// and update the bank's capitalization.
    pub fn minimize(
        bank: &'a Bank,
        starting_slot: Slot,
        transaction_account_set: DashSet<Pubkey>,
        should_recalculate_accounts_lt_hash: bool,
    ) {
        let minimizer = SnapshotMinimizer {
            bank,
            starting_slot,
            minimized_account_set: transaction_account_set,
        };

        minimizer.add_accounts(Self::get_active_bank_features, "active bank features");
        minimizer.add_accounts(Self::get_inactive_bank_features, "inactive bank features");
        minimizer.add_accounts(Self::get_static_runtime_accounts, "static runtime accounts");
        minimizer.add_accounts(Self::get_reserved_accounts, "reserved accounts");

        minimizer.add_accounts(Self::get_vote_accounts, "vote accounts");
        minimizer.add_accounts(Self::get_stake_accounts, "stake accounts");
        minimizer.add_accounts(Self::get_owner_accounts, "owner accounts");
        minimizer.add_accounts(Self::get_programdata_accounts, "programdata accounts");

        minimizer.minimize_accounts_db();

        // Update accounts_cache and capitalization
        minimizer.bank.force_flush_accounts_cache();
        minimizer
            .bank
            .set_capitalization_for_tests(minimizer.bank.calculate_capitalization_for_tests());

        if should_recalculate_accounts_lt_hash {
            // Since the account state has changed, the accounts lt hash must be recalculated
            let new_accounts_lt_hash = minimizer
                .accounts_db()
                .calculate_accounts_lt_hash_at_startup_from_index(
                    &minimizer.bank.ancestors,
                    minimizer.bank.slot(),
                );
            bank.set_accounts_lt_hash_for_snapshot_minimizer(new_accounts_lt_hash);
        }
    }

    /// Helper function to measure time and number of accounts added
    fn add_accounts<F>(&self, add_accounts_fn: F, name: &'static str)
    where
        F: Fn(&SnapshotMinimizer<'a>),
    {
        let initial_accounts_len = self.minimized_account_set.len();
        let (_, measure) = measure_time!(add_accounts_fn(self), name);
        let total_accounts_len = self.minimized_account_set.len();
        let added_accounts = total_accounts_len - initial_accounts_len;

        info!(
            "Added {added_accounts} {name} for total of {total_accounts_len} accounts. get \
             {measure}"
        );
    }

    /// Used to get active bank feature accounts in `minimize`.
    fn get_active_bank_features(&self) {
        self.bank
            .feature_set
            .active()
            .iter()
            .for_each(|(pubkey, _)| {
                self.minimized_account_set.insert(*pubkey);
            });
    }

    /// Used to get inactive bank feature accounts in `minimize`
    fn get_inactive_bank_features(&self) {
        self.bank.feature_set.inactive().iter().for_each(|pubkey| {
            self.minimized_account_set.insert(*pubkey);
        });
    }

    /// Used to get static runtime accounts in `minimize`
    fn get_static_runtime_accounts(&self) {
        static_ids::STATIC_IDS.iter().for_each(|pubkey| {
            self.minimized_account_set.insert(*pubkey);
        });
    }

    /// Used to get reserved accounts in `minimize`
    fn get_reserved_accounts(&self) {
        ReservedAccountKeys::all_keys_iter().for_each(|pubkey| {
            self.minimized_account_set.insert(*pubkey);
        })
    }

    /// Used to get vote and node pubkeys in `minimize`
    /// Add all pubkeys from vote accounts and nodes to `minimized_account_set`
    fn get_vote_accounts(&self) {
        self.bank
            .vote_accounts()
            .par_iter()
            .for_each(|(pubkey, (_stake, vote_account))| {
                self.minimized_account_set.insert(*pubkey);
                self.minimized_account_set
                    .insert(*vote_account.node_pubkey());
            });
    }

    /// Used to get stake accounts in `minimize`
    /// Add all pubkeys from stake accounts to `minimized_account_set`
    fn get_stake_accounts(&self) {
        self.bank.get_stake_accounts(&self.minimized_account_set);
    }

    /// Used to get owner accounts in `minimize`
    /// For each account in `minimized_account_set` adds the owner account's pubkey to `minimized_account_set`.
    fn get_owner_accounts(&self) {
        let owner_accounts: HashSet<_> = self
            .minimized_account_set
            .par_iter()
            .filter_map(|pubkey| self.bank.get_account(&pubkey))
            .map(|account| *account.owner())
            .collect();
        owner_accounts.into_par_iter().for_each(|pubkey| {
            self.minimized_account_set.insert(pubkey);
        });
    }

    /// Used to get program data accounts in `minimize`
    /// For each upgradable bpf program, adds the programdata account pubkey to `minimized_account_set`
    fn get_programdata_accounts(&self) {
        let programdata_accounts: HashSet<_> = self
            .minimized_account_set
            .par_iter()
            .filter_map(|pubkey| self.bank.get_account(&pubkey))
            .filter(|account| account.executable())
            .filter(|account| bpf_loader_upgradeable::check_id(account.owner()))
            .filter_map(|account| {
                if let Ok(UpgradeableLoaderState::Program {
                    programdata_address,
                }) = account.state()
                {
                    Some(programdata_address)
                } else {
                    None
                }
            })
            .collect();
        programdata_accounts.into_par_iter().for_each(|pubkey| {
            self.minimized_account_set.insert(pubkey);
        });
    }

    /// Remove accounts not in `minimized_accoun_set` from accounts_db
    fn minimize_accounts_db(&self) {
        let (minimized_slot_set, minimized_slot_set_measure) =
            measure_time!(self.get_minimized_slot_set(), "generate minimized slot set");
        info!("{minimized_slot_set_measure}");

        let ((dead_slots, dead_storages), process_snapshot_storages_measure) = measure_time!(
            self.process_snapshot_storages(minimized_slot_set),
            "process snapshot storages"
        );
        info!("{process_snapshot_storages_measure}");

        // Avoid excessive logging
        self.accounts_db()
            .log_dead_slots
            .store(false, Ordering::Relaxed);

        let (_, purge_dead_slots_measure) =
            measure_time!(self.purge_dead_slots(dead_slots), "purge dead slots");
        info!("{purge_dead_slots_measure}");

        let (_, drop_storages_measure) = measure_time!(drop(dead_storages), "drop storages");
        info!("{drop_storages_measure}");

        // Turn logging back on after minimization
        self.accounts_db()
            .log_dead_slots
            .store(true, Ordering::Relaxed);
    }

    /// Determines minimum set of slots that accounts in `minimized_account_set` are in
    fn get_minimized_slot_set(&self) -> DashSet<Slot> {
        let minimized_slot_set = DashSet::new();
        self.minimized_account_set.par_iter().for_each(|pubkey| {
            self.accounts_db()
                .accounts_index
                .get_and_then(&pubkey, |entry| {
                    if let Some(entry) = entry {
                        let max_slot = entry
                            .slot_list_read_lock()
                            .iter()
                            .map(|(slot, _)| *slot)
                            .max();
                        if let Some(max_slot) = max_slot {
                            minimized_slot_set.insert(max_slot);
                        }
                    }
                    (false, ())
                });
        });
        minimized_slot_set
    }

    /// Process all snapshot storages to during `minimize`
    fn process_snapshot_storages(
        &self,
        minimized_slot_set: DashSet<Slot>,
    ) -> (Vec<Slot>, Vec<Arc<AccountStorageEntry>>) {
        let snapshot_storages = self.accounts_db().get_storages(..=self.starting_slot).0;

        let dead_slots = Mutex::new(Vec::new());
        let dead_storages = Mutex::new(Vec::new());

        snapshot_storages.into_par_iter().for_each(|storage| {
            let slot = storage.slot();
            if slot != self.starting_slot {
                if minimized_slot_set.contains(&slot) {
                    self.filter_storage(&storage, &dead_storages);
                } else {
                    dead_slots.lock().unwrap().push(slot);
                }
            }
        });

        let dead_slots = dead_slots.into_inner().unwrap();
        let dead_storages = dead_storages.into_inner().unwrap();
        (dead_slots, dead_storages)
    }

    /// Creates new storage replacing `storages` that contains only accounts in `minimized_account_set`.
    fn filter_storage(
        &self,
        storage: &Arc<AccountStorageEntry>,
        dead_storages: &Mutex<Vec<Arc<AccountStorageEntry>>>,
    ) {
        let slot = storage.slot();
        let GetUniqueAccountsResult {
            stored_accounts, ..
        } = self.accounts_db().get_unique_accounts_from_storage(storage);

        let keep_accounts_collect = Mutex::new(Vec::with_capacity(stored_accounts.len()));
        let purge_pubkeys_collect = Mutex::new(Vec::with_capacity(stored_accounts.len()));
        let total_bytes_collect = AtomicUsize::new(0);
        const CHUNK_SIZE: usize = 50;
        stored_accounts.par_chunks(CHUNK_SIZE).for_each(|chunk| {
            let mut chunk_bytes = 0;
            let mut keep_accounts = Vec::with_capacity(CHUNK_SIZE);
            let mut purge_pubkeys = Vec::with_capacity(CHUNK_SIZE);
            chunk.iter().for_each(|account| {
                if self.minimized_account_set.contains(account.pubkey()) {
                    chunk_bytes += account.stored_size();
                    keep_accounts.push(account);
                } else if self.accounts_db().accounts_index.contains(account.pubkey()) {
                    purge_pubkeys.push(account.pubkey());
                }
            });

            keep_accounts_collect
                .lock()
                .unwrap()
                .append(&mut keep_accounts);
            purge_pubkeys_collect
                .lock()
                .unwrap()
                .append(&mut purge_pubkeys);
            total_bytes_collect.fetch_add(chunk_bytes, Ordering::Relaxed);
        });

        let keep_accounts = keep_accounts_collect.into_inner().unwrap();
        let remove_pubkeys = purge_pubkeys_collect.into_inner().unwrap();
        let total_bytes = total_bytes_collect.load(Ordering::Relaxed);

        let purge_pubkeys = remove_pubkeys.into_iter().map(|pubkey| (*pubkey, slot));
        let _ = self.accounts_db().purge_keys_exact(purge_pubkeys);

        let mut shrink_in_progress = None;
        if total_bytes > 0 {
            shrink_in_progress = Some(
                self.accounts_db()
                    .get_store_for_shrink(slot, total_bytes as u64),
            );
            let new_storage = shrink_in_progress.as_ref().unwrap().new_storage();

            let accounts = [(slot, &keep_accounts[..])];
            let storable_accounts =
                StorableAccountsBySlot::new(slot, &accounts, self.accounts_db());

            self.accounts_db().store_accounts_frozen(
                storable_accounts,
                new_storage,
                UpdateIndexThreadSelection::Inline,
            );

            new_storage.flush().unwrap();
        }

        let mut dead_storages_this_time = self.accounts_db().mark_dirty_dead_stores(
            slot,
            true, // add_dirty_stores
            shrink_in_progress,
            false,
        );
        dead_storages
            .lock()
            .unwrap()
            .append(&mut dead_storages_this_time);
    }

    /// Purge dead slots from storage and cache
    fn purge_dead_slots(&self, dead_slots: Vec<Slot>) {
        let stats = PurgeStats::default();
        self.accounts_db()
            .purge_slots_from_cache_and_store(dead_slots.iter(), &stats);
    }

    /// Convenience function for getting accounts_db
    fn accounts_db(&self) -> &AccountsDb {
        &self.bank.rc.accounts.accounts_db
    }
}

#[cfg(test)]
mod tests {
    use {
        crate::{
            bank::Bank,
            genesis_utils::{self, create_genesis_config_with_leader},
            runtime_config::RuntimeConfig,
            snapshot_bank_utils,
            snapshot_minimizer::SnapshotMinimizer,
            snapshot_utils,
        },
        agave_snapshots::snapshot_config::SnapshotConfig,
        dashmap::DashSet,
        solana_account::{AccountSharedData, ReadableAccount, WritableAccount},
        solana_accounts_db::accounts_db::{AccountsDbConfig, ACCOUNTS_DB_CONFIG_FOR_TESTING},
        solana_genesis_config::create_genesis_config,
        solana_loader_v3_interface::state::UpgradeableLoaderState,
        solana_pubkey::Pubkey,
        solana_sdk_ids::bpf_loader_upgradeable,
        solana_signer::Signer,
        solana_stake_interface as stake,
        std::sync::Arc,
        tempfile::TempDir,
        test_case::test_case,
    };

    #[test]
    fn test_minimization_get_vote_accounts() {
        agave_logger::setup();

        let bootstrap_validator_pubkey = solana_pubkey::new_rand();
        let bootstrap_validator_stake_lamports = 30;
        let genesis_config_info = create_genesis_config_with_leader(
            10,
            &bootstrap_validator_pubkey,
            bootstrap_validator_stake_lamports,
        );

        let bank = Arc::new(Bank::new_for_tests(&genesis_config_info.genesis_config));

        let minimizer = SnapshotMinimizer {
            bank: &bank,
            starting_slot: 0,
            minimized_account_set: DashSet::new(),
        };
        minimizer.get_vote_accounts();

        assert!(minimizer
            .minimized_account_set
            .contains(&genesis_config_info.voting_keypair.pubkey()));
        assert!(minimizer
            .minimized_account_set
            .contains(&genesis_config_info.validator_pubkey));
    }

    #[test]
    fn test_minimization_get_stake_accounts() {
        agave_logger::setup();

        let bootstrap_validator_pubkey = solana_pubkey::new_rand();
        let bootstrap_validator_stake_lamports = 30;
        let genesis_config_info = create_genesis_config_with_leader(
            10,
            &bootstrap_validator_pubkey,
            bootstrap_validator_stake_lamports,
        );

        let bank = Arc::new(Bank::new_for_tests(&genesis_config_info.genesis_config));
        let minimizer = SnapshotMinimizer {
            bank: &bank,
            starting_slot: 0,
            minimized_account_set: DashSet::new(),
        };
        minimizer.get_stake_accounts();

        let mut expected_stake_accounts: Vec<_> = genesis_config_info
            .genesis_config
            .accounts
            .iter()
            .filter_map(|(pubkey, account)| {
                stake::program::check_id(account.owner()).then_some(*pubkey)
            })
            .collect();
        expected_stake_accounts.push(bootstrap_validator_pubkey);

        assert_eq!(
            minimizer.minimized_account_set.len(),
            expected_stake_accounts.len()
        );
        for stake_pubkey in expected_stake_accounts {
            assert!(minimizer.minimized_account_set.contains(&stake_pubkey));
        }
    }

    #[test]
    fn test_minimization_get_owner_accounts() {
        agave_logger::setup();

        let (genesis_config, _) = create_genesis_config(1_000_000);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));

        let pubkey = solana_pubkey::new_rand();
        let owner_pubkey = solana_pubkey::new_rand();
        bank.store_account(&pubkey, &AccountSharedData::new(1, 0, &owner_pubkey));

        let owner_accounts = DashSet::new();
        owner_accounts.insert(pubkey);
        let minimizer = SnapshotMinimizer {
            bank: &bank,
            starting_slot: 0,
            minimized_account_set: owner_accounts,
        };

        minimizer.get_owner_accounts();
        assert!(minimizer.minimized_account_set.contains(&pubkey));
        assert!(minimizer.minimized_account_set.contains(&owner_pubkey));
    }

    #[test]
    fn test_minimization_add_programdata_accounts() {
        agave_logger::setup();

        let (genesis_config, _) = create_genesis_config(1_000_000);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));

        let non_program_id = solana_pubkey::new_rand();
        let program_id = solana_pubkey::new_rand();
        let programdata_address = solana_pubkey::new_rand();

        let program = UpgradeableLoaderState::Program {
            programdata_address,
        };

        let non_program_account = AccountSharedData::new(1, 0, &non_program_id);
        let mut program_account =
            AccountSharedData::new_data(40, &program, &bpf_loader_upgradeable::id()).unwrap();
        program_account.set_executable(true);

        bank.store_account(&non_program_id, &non_program_account);
        bank.store_account(&program_id, &program_account);

        // Non-program account does not add any additional keys
        let programdata_accounts = DashSet::new();
        programdata_accounts.insert(non_program_id);
        let minimizer = SnapshotMinimizer {
            bank: &bank,
            starting_slot: 0,
            minimized_account_set: programdata_accounts,
        };
        minimizer.get_programdata_accounts();
        assert_eq!(minimizer.minimized_account_set.len(), 1);
        assert!(minimizer.minimized_account_set.contains(&non_program_id));

        // Programdata account adds the programdata address to the set
        minimizer.minimized_account_set.insert(program_id);
        minimizer.get_programdata_accounts();
        assert_eq!(minimizer.minimized_account_set.len(), 3);
        assert!(minimizer.minimized_account_set.contains(&non_program_id));
        assert!(minimizer.minimized_account_set.contains(&program_id));
        assert!(minimizer
            .minimized_account_set
            .contains(&programdata_address));
    }

    #[test]
    fn test_minimize_accounts_db() {
        agave_logger::setup();

        let (genesis_config, _) = create_genesis_config(1_000_000);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let accounts = &bank.accounts().accounts_db;

        let num_slots = 5;
        let num_accounts_per_slot = 300;

        let mut current_slot = 0;
        let minimized_account_set = DashSet::new();
        for _ in 0..num_slots {
            let pubkeys: Vec<_> = (0..num_accounts_per_slot)
                .map(|_| solana_pubkey::new_rand())
                .collect();

            let some_lamport = 223;
            let no_data = 0;
            let owner = *AccountSharedData::default().owner();
            let account = AccountSharedData::new(some_lamport, no_data, &owner);

            current_slot += 1;

            for (index, pubkey) in pubkeys.iter().enumerate() {
                accounts.store_for_tests((current_slot, [(pubkey, &account)].as_slice()));

                if current_slot % 2 == 0 && index % 100 == 0 {
                    minimized_account_set.insert(*pubkey);
                }
            }
            accounts.add_root_and_flush_write_cache(current_slot);
        }

        assert_eq!(minimized_account_set.len(), 6);
        let minimizer = SnapshotMinimizer {
            bank: &bank,
            starting_slot: current_slot,
            minimized_account_set,
        };
        minimizer.minimize_accounts_db();

        let snapshot_storages = accounts.get_storages(..=current_slot).0;
        assert_eq!(snapshot_storages.len(), 3);

        let mut account_count = 0;
        snapshot_storages.into_iter().for_each(|storage| {
            storage
                .accounts
                .scan_pubkeys(|_| {
                    account_count += 1;
                })
                .expect("must scan accounts storage");
        });

        assert_eq!(
            account_count,
            minimizer.minimized_account_set.len() + num_accounts_per_slot
        ); // snapshot slot is untouched, so still has all 300 accounts
    }

    /// Ensure that minimized snapshots are loadable with and without
    /// recalculating the accounts lt hash.
    #[test_case(false)]
    #[test_case(true)]
    fn test_minimize_and_recalculate_accounts_lt_hash(should_recalculate_accounts_lt_hash: bool) {
        let genesis_config_info = genesis_utils::create_genesis_config(123_456_789_000_000_000);
        let (bank, bank_forks) =
            Bank::new_with_bank_forks_for_tests(&genesis_config_info.genesis_config);

        // write to multiple accounts and keep track of one, for minimization later
        let pubkey_to_keep = Pubkey::new_unique();
        let slot = bank.slot() + 1;
        let bank = Bank::new_from_parent(bank, &Pubkey::default(), slot);
        let bank = bank_forks
            .write()
            .unwrap()
            .insert(bank)
            .clone_without_scheduler();
        bank.register_unique_recent_blockhash_for_test();
        bank.transfer(
            1_000_000_000,
            &genesis_config_info.mint_keypair,
            &Pubkey::new_unique(),
        )
        .unwrap();
        bank.transfer(
            1_000_000_000,
            &genesis_config_info.mint_keypair,
            &pubkey_to_keep,
        )
        .unwrap();
        bank.fill_bank_with_ticks_for_tests();
        bank.squash();
        bank.force_flush_accounts_cache();

        // do the minimization
        SnapshotMinimizer::minimize(
            &bank,
            bank.slot(),
            DashSet::from_iter([pubkey_to_keep]),
            should_recalculate_accounts_lt_hash,
        );

        // take a snapshot of the minimized bank, then load it
        let snapshot_config = SnapshotConfig::default();
        let bank_snapshots_dir = TempDir::new().unwrap();
        let snapshot_archives_dir = TempDir::new().unwrap();
        let snapshot = snapshot_bank_utils::bank_to_full_snapshot_archive(
            &bank_snapshots_dir,
            &bank,
            Some(snapshot_config.snapshot_version),
            &snapshot_archives_dir,
            &snapshot_archives_dir,
            snapshot_config.archive_format,
        )
        .unwrap();
        let (_accounts_tempdir, accounts_dir) = snapshot_utils::create_tmp_accounts_dir_for_tests();
        let accounts_db_config = AccountsDbConfig {
            // must skip accounts verification if we did not recalculate the accounts lt hash
            skip_initial_hash_calc: !should_recalculate_accounts_lt_hash,
            ..ACCOUNTS_DB_CONFIG_FOR_TESTING
        };
        let roundtrip_bank = snapshot_bank_utils::bank_from_snapshot_archives(
            &[accounts_dir],
            &bank_snapshots_dir,
            &snapshot,
            None,
            &genesis_config_info.genesis_config,
            &RuntimeConfig::default(),
            None,
            None,
            false,
            false,
            false,
            accounts_db_config,
            None,
            Arc::default(),
        )
        .unwrap();

        assert_eq!(roundtrip_bank, *bank);
    }
}
