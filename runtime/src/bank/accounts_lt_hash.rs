use {
    super::Bank,
    log::{log_enabled, trace, Level},
    solana_accounts_db::{accounts_db::AccountsDb, storable_accounts::StorableAccounts},
    solana_lattice_hash::lt_hash::LtHash,
};

impl Bank {
    /// Updates the accounts lt hash, inline with transaction processing
    ///
    /// Note that it is critical this fn is called *BEFORE* `accounts` have actually been stored!
    ///
    /// Given `accounts` that are about to be stored, mix out each account's previous state and mix
    /// in each account's new state from this bank's accounts lt hash.
    pub fn update_accounts_lt_hash<'a>(&self, accounts: &impl StorableAccounts<'a>) {
        let slot = self.slot();
        let num_accounts = accounts.len();
        let mut delta_lt_hash = LtHash::identity();
        for i in 0..num_accounts {
            let pubkey = accounts.pubkey(i);

            // MIX OUT
            let prev_account = self
                .rc
                .accounts
                .load_with_fixed_root_do_not_populate_read_cache(&self.ancestors, pubkey);
            if let Some((prev_account, _prev_slot)) = prev_account {
                let prev_lt_hash = AccountsDb::lt_hash_account(&prev_account, pubkey);
                delta_lt_hash.mix_out(&prev_lt_hash.0);
                if log_enabled!(Level::Trace) {
                    trace!(
                        "lt hash mix out, slot {slot}, {pubkey}, checksum: {}, {prev_account:?}",
                        prev_lt_hash.0.checksum(),
                    );
                }
            } else {
                // no previous account, so nothing to mix out; just log
                if log_enabled!(Level::Trace) {
                    trace!("lt hash mix out, slot {slot}, {pubkey}, account not found");
                }
            }

            // MIX IN
            accounts.account(i, |post_account| {
                let post_lt_hash = AccountsDb::lt_hash_account(&post_account, pubkey);
                delta_lt_hash.mix_in(&post_lt_hash.0);
                if log_enabled!(Level::Trace) {
                    trace!(
                        "lt hash mix in,  slot {slot}, {pubkey}, checksum: {}, {post_account:?}",
                        post_lt_hash.0.checksum(),
                    );
                }
            });
        }

        // only lock the accounts_lt_hash mutex if there are actual account updates
        if delta_lt_hash != LtHash::identity() {
            let mut accounts_lt_hash = self.accounts_lt_hash.lock().unwrap();
            accounts_lt_hash.0.mix_in(&delta_lt_hash);
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            bank::tests::new_bank_from_parent_with_bank_forks, runtime_config::RuntimeConfig,
            snapshot_bank_utils, snapshot_config::SnapshotConfig, snapshot_utils,
        },
        solana_account::AccountSharedData,
        solana_accounts_db::{
            accounts_db::{AccountsDbConfig, MarkObsoleteAccounts, ACCOUNTS_DB_CONFIG_FOR_TESTING},
            accounts_index::{
                AccountsIndexConfig, IndexLimitMb, ACCOUNTS_INDEX_CONFIG_FOR_TESTING,
            },
        },
        solana_fee_calculator::FeeRateGovernor,
        solana_genesis_config::{self, GenesisConfig},
        solana_keypair::Keypair,
        solana_native_token::LAMPORTS_PER_SOL,
        solana_pubkey::{self as pubkey, Pubkey},
        solana_signer::Signer as _,
        std::{cmp, iter, str::FromStr as _, sync::Arc},
        tempfile::TempDir,
        test_case::{test_case, test_matrix},
    };

    /// What features should be enabled?
    #[derive(Debug, Copy, Clone, Eq, PartialEq)]
    enum Features {
        /// Do not enable any features
        None,
        /// Enable all features
        All,
    }

    /// Creates a genesis config with `features` enabled
    fn genesis_config_with(features: Features) -> (GenesisConfig, Keypair) {
        let mint_lamports = 123_456_789 * LAMPORTS_PER_SOL;
        match features {
            Features::None => solana_genesis_config::create_genesis_config(mint_lamports),
            Features::All => {
                let info = crate::genesis_utils::create_genesis_config(mint_lamports);
                (info.genesis_config, info.mint_keypair)
            }
        }
    }

    #[test]
    fn test_update_accounts_lt_hash() {
        // Write to address 1, 2, and 5 in first bank, so that in second bank we have
        // updates to these three accounts.  Make address 2 go to zero (dead).  Make address 1 and 3 stay
        // alive.  Make address 5 unchanged.  Ensure the updates are expected.
        //
        // 1: alive -> alive
        // 2: alive -> dead
        // 3: dead -> alive
        // 4. dead -> dead
        // 5. alive -> alive *unchanged*

        let keypair1 = Keypair::new();
        let keypair2 = Keypair::new();
        let keypair3 = Keypair::new();
        let keypair4 = Keypair::new();
        let keypair5 = Keypair::new();

        let (mut genesis_config, mint_keypair) =
            solana_genesis_config::create_genesis_config(123_456_789 * LAMPORTS_PER_SOL);
        genesis_config.fee_rate_governor = FeeRateGovernor::new(0, 0);
        let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        let amount = cmp::max(
            bank.get_minimum_balance_for_rent_exemption(0),
            LAMPORTS_PER_SOL,
        );

        // send lamports to accounts 1, 2, and 5 so they are alive,
        // and so we'll have a delta in the next bank
        bank.register_unique_recent_blockhash_for_test();
        bank.transfer(amount, &mint_keypair, &keypair1.pubkey())
            .unwrap();
        bank.transfer(amount, &mint_keypair, &keypair2.pubkey())
            .unwrap();
        bank.transfer(amount, &mint_keypair, &keypair5.pubkey())
            .unwrap();

        // freeze the bank to ensure deferred account state changes are completed
        // e.g. update the slot history sysvar
        bank.freeze();
        let prev_accounts_lt_hash = bank.accounts_lt_hash.lock().unwrap().clone();

        // save the initial values of the accounts to use for asserts later
        let prev_mint = bank.get_account_with_fixed_root(&mint_keypair.pubkey());
        let prev_account1 = bank.get_account_with_fixed_root(&keypair1.pubkey());
        let prev_account2 = bank.get_account_with_fixed_root(&keypair2.pubkey());
        let prev_account3 = bank.get_account_with_fixed_root(&keypair3.pubkey());
        let prev_account4 = bank.get_account_with_fixed_root(&keypair4.pubkey());
        let prev_account5 = bank.get_account_with_fixed_root(&keypair5.pubkey());

        assert!(prev_mint.is_some());
        assert!(prev_account1.is_some());
        assert!(prev_account2.is_some());
        assert!(prev_account3.is_none());
        assert!(prev_account4.is_none());
        assert!(prev_account5.is_some());

        // These sysvars are also updated, but outside of transaction processing.
        let sysvars = [
            Pubkey::from_str("SysvarS1otHashes111111111111111111111111111").unwrap(),
            Pubkey::from_str("SysvarC1ock11111111111111111111111111111111").unwrap(),
            Pubkey::from_str("SysvarRecentB1ockHashes11111111111111111111").unwrap(),
            Pubkey::from_str("SysvarS1otHistory11111111111111111111111111").unwrap(),
        ];
        let prev_sysvar_accounts: Vec<_> = sysvars
            .iter()
            .map(|address| bank.get_account_with_fixed_root(address))
            .collect();

        let bank = {
            let slot = bank.slot() + 1;
            new_bank_from_parent_with_bank_forks(&bank_forks, bank, &Pubkey::default(), slot)
        };

        // send from account 2 to account 1; account 1 stays alive, account 2 ends up dead
        bank.register_unique_recent_blockhash_for_test();
        bank.transfer(amount, &keypair2, &keypair1.pubkey())
            .unwrap();

        // send lamports to account 1 in multiple transactions to exercise updating an
        // account multiple times in the same block
        bank.register_unique_recent_blockhash_for_test();
        bank.transfer(amount, &mint_keypair, &keypair1.pubkey())
            .unwrap();
        bank.register_unique_recent_blockhash_for_test();
        bank.transfer(amount, &mint_keypair, &keypair1.pubkey())
            .unwrap();

        // send lamports to account 4, then turn around and send them to account 3
        // account 3 will be alive, and account 4 will end dead
        bank.register_unique_recent_blockhash_for_test();
        bank.transfer(amount, &mint_keypair, &keypair4.pubkey())
            .unwrap();
        bank.register_unique_recent_blockhash_for_test();
        bank.transfer(amount, &keypair4, &keypair3.pubkey())
            .unwrap();

        // store account 5 into this new bank, unchanged
        bank.store_account(&keypair5.pubkey(), prev_account5.as_ref().unwrap());

        // freeze the bank to ensure deferred account state changes are completed
        // e.g. update the slot history sysvar
        bank.freeze();

        let post_accounts_lt_hash = bank.accounts_lt_hash.lock().unwrap().clone();
        let post_mint = bank.get_account_with_fixed_root(&mint_keypair.pubkey());
        let post_account1 = bank.get_account_with_fixed_root(&keypair1.pubkey());
        let post_account2 = bank.get_account_with_fixed_root(&keypair2.pubkey());
        let post_account3 = bank.get_account_with_fixed_root(&keypair3.pubkey());
        let post_account4 = bank.get_account_with_fixed_root(&keypair4.pubkey());
        let post_account5 = bank.get_account_with_fixed_root(&keypair5.pubkey());

        assert!(post_mint.is_some());
        assert!(post_account1.is_some());
        assert!(post_account2.is_none());
        assert!(post_account3.is_some());
        assert!(post_account4.is_none());
        assert!(post_account5.is_some());

        let post_sysvar_accounts: Vec<_> = sysvars
            .iter()
            .map(|address| bank.get_account_with_fixed_root(address))
            .collect();

        let mut expected_accounts_lt_hash = prev_accounts_lt_hash.clone();
        let mut updater =
            |address: &Pubkey, prev: Option<AccountSharedData>, post: Option<AccountSharedData>| {
                // if there was an alive account, mix out
                if let Some(prev) = prev {
                    let prev_lt_hash = AccountsDb::lt_hash_account(&prev, address);
                    expected_accounts_lt_hash.0.mix_out(&prev_lt_hash.0);
                }

                // mix in the new one
                let post = post.unwrap_or_default();
                let post_lt_hash = AccountsDb::lt_hash_account(&post, address);
                expected_accounts_lt_hash.0.mix_in(&post_lt_hash.0);
            };
        updater(&mint_keypair.pubkey(), prev_mint, post_mint);
        updater(&keypair1.pubkey(), prev_account1, post_account1);
        updater(&keypair2.pubkey(), prev_account2, post_account2);
        updater(&keypair3.pubkey(), prev_account3, post_account3);
        updater(&keypair4.pubkey(), prev_account4, post_account4);
        updater(&keypair5.pubkey(), prev_account5, post_account5);
        for (i, sysvar) in sysvars.iter().enumerate() {
            updater(
                sysvar,
                prev_sysvar_accounts[i].clone(),
                post_sysvar_accounts[i].clone(),
            );
        }

        // now make sure the accounts lt hashes match
        assert_eq!(
            expected_accounts_lt_hash,
            post_accounts_lt_hash,
            "accounts_lt_hash, expected: {}, actual: {}",
            expected_accounts_lt_hash.0.checksum(),
            post_accounts_lt_hash.0.checksum(),
        );
    }

    /// Ensure that the accounts lt hash is correct for slot 0
    ///
    /// This test does a simple transfer in slot 0 so that a primordial account is modified.
    #[test_case(Features::None; "no features")]
    #[test_case(Features::All; "all features")]
    fn test_slot0_accounts_lt_hash(features: Features) {
        let (genesis_config, mint_keypair) = genesis_config_with(features);
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        // ensure this bank is for slot 0, otherwise this test doesn't actually do anything...
        assert_eq!(bank.slot(), 0);

        // process a transaction that modifies a primordial account
        bank.transfer(LAMPORTS_PER_SOL, &mint_keypair, &Pubkey::new_unique())
            .unwrap();

        // freeze the bank to ensure deferred account state changes are completed
        // (not strictly needed for this test, but does make it more comprehensive)
        bank.freeze();
        let actual_accounts_lt_hash = bank.accounts_lt_hash.lock().unwrap().clone();

        // ensure the actual accounts lt hash matches the value calculated from the index
        let calculated_accounts_lt_hash = bank
            .rc
            .accounts
            .accounts_db
            .calculate_accounts_lt_hash_at_startup_from_index(&bank.ancestors, bank.slot());
        assert_eq!(actual_accounts_lt_hash, calculated_accounts_lt_hash);
    }

    #[test_case(Features::None; "no features")]
    #[test_case(Features::All; "all features")]
    fn test_calculate_accounts_lt_hash_at_startup_from_index(features: Features) {
        let (genesis_config, mint_keypair) = genesis_config_with(features);
        let (mut bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        let amount = cmp::max(
            bank.get_minimum_balance_for_rent_exemption(0),
            LAMPORTS_PER_SOL,
        );

        // create some banks with some modified accounts so that there are stored accounts
        // (note: the number of banks and transfers are arbitrary)
        for _ in 0..7 {
            let slot = bank.slot() + 1;
            bank =
                new_bank_from_parent_with_bank_forks(&bank_forks, bank, &Pubkey::default(), slot);
            for _ in 0..13 {
                bank.register_unique_recent_blockhash_for_test();
                // note: use a random pubkey here to ensure accounts
                // are spread across all the index bins
                bank.transfer(amount, &mint_keypair, &pubkey::new_rand())
                    .unwrap();
            }
            bank.freeze();
        }
        let expected_accounts_lt_hash = bank.accounts_lt_hash.lock().unwrap().clone();

        // root the bank and flush the accounts write cache to disk
        // (this more accurately simulates startup, where accounts are in storages on disk)
        bank.squash();
        bank.force_flush_accounts_cache();

        // call the fn that calculates the accounts lt hash at startup, then ensure it matches
        let calculated_accounts_lt_hash = bank
            .rc
            .accounts
            .accounts_db
            .calculate_accounts_lt_hash_at_startup_from_index(&bank.ancestors, bank.slot());
        assert_eq!(expected_accounts_lt_hash, calculated_accounts_lt_hash);
    }

    #[test_matrix(
        [Features::None, Features::All],
        [IndexLimitMb::Minimal, IndexLimitMb::InMemOnly],
        [MarkObsoleteAccounts::Disabled, MarkObsoleteAccounts::Enabled]
    )]
    fn test_verify_accounts_lt_hash_at_startup(
        features: Features,
        accounts_index_limit: IndexLimitMb,
        mark_obsolete_accounts: MarkObsoleteAccounts,
    ) {
        let (mut genesis_config, mint_keypair) = genesis_config_with(features);
        // This test requires zero fees so that we can easily transfer an account's entire balance.
        genesis_config.fee_rate_governor = FeeRateGovernor::new(0, 0);
        let (mut bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        let amount = cmp::max(
            bank.get_minimum_balance_for_rent_exemption(0),
            LAMPORTS_PER_SOL,
        );

        // Write to this pubkey multiple times, so there are guaranteed duplicates in the storages.
        let duplicate_pubkey = pubkey::new_rand();

        // create some banks with some modified accounts so that there are stored accounts
        // (note: the number of banks and transfers are arbitrary)
        for _ in 0..9 {
            let slot = bank.slot() + 1;
            bank =
                new_bank_from_parent_with_bank_forks(&bank_forks, bank, &Pubkey::default(), slot);
            for _ in 0..3 {
                bank.register_unique_recent_blockhash_for_test();
                bank.transfer(amount, &mint_keypair, &pubkey::new_rand())
                    .unwrap();
                bank.register_unique_recent_blockhash_for_test();
                bank.transfer(amount, &mint_keypair, &duplicate_pubkey)
                    .unwrap();
            }

            // flush the write cache to disk to ensure there are duplicates across the storages
            bank.fill_bank_with_ticks_for_tests();
            bank.squash();
            bank.force_flush_accounts_cache();
        }

        // Create a few more storages to exercise the zero lamport duplicates handling during
        // generate_index(), which is used for the lattice-based accounts verification.
        // There needs to be accounts that only have a single duplicate (i.e. there are only two
        // versions of the accounts), and toggle between non-zero and zero lamports.
        // One account will go zero -> non-zero, and the other will go non-zero -> zero.
        let num_accounts = 2;
        let accounts: Vec<_> = iter::repeat_with(Keypair::new).take(num_accounts).collect();
        for i in 0..num_accounts {
            let slot = bank.slot() + 1;
            bank =
                new_bank_from_parent_with_bank_forks(&bank_forks, bank, &Pubkey::default(), slot);
            bank.register_unique_recent_blockhash_for_test();

            // transfer into the accounts so they start with a non-zero balance
            for account in &accounts {
                bank.transfer(amount, &mint_keypair, &account.pubkey())
                    .unwrap();
                assert_ne!(bank.get_balance(&account.pubkey()), 0);
            }

            // then transfer *out* all the lamports from one of 'em
            bank.transfer(
                bank.get_balance(&accounts[i].pubkey()),
                &accounts[i],
                &pubkey::new_rand(),
            )
            .unwrap();
            assert_eq!(bank.get_balance(&accounts[i].pubkey()), 0);

            // flush the write cache to disk to ensure the storages match the accounts written here
            bank.fill_bank_with_ticks_for_tests();
            bank.squash();
            bank.force_flush_accounts_cache();
        }

        // verification happens at startup, so mimic the behavior by loading from a snapshot
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
        let accounts_index_config = AccountsIndexConfig {
            index_limit_mb: accounts_index_limit,
            ..ACCOUNTS_INDEX_CONFIG_FOR_TESTING
        };
        let accounts_db_config = AccountsDbConfig {
            index: Some(accounts_index_config),
            mark_obsolete_accounts,
            ..ACCOUNTS_DB_CONFIG_FOR_TESTING
        };
        let roundtrip_bank = snapshot_bank_utils::bank_from_snapshot_archives(
            &[accounts_dir],
            &bank_snapshots_dir,
            &snapshot,
            None,
            &genesis_config,
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

    /// Ensure that the snapshot hash is correct
    #[test_case(Features::None; "no features")]
    #[test_case(Features::All; "all features")]
    fn test_snapshots(features: Features) {
        let (genesis_config, mint_keypair) = genesis_config_with(features);
        let (mut bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        let amount = cmp::max(
            bank.get_minimum_balance_for_rent_exemption(0),
            LAMPORTS_PER_SOL,
        );

        // create some banks with some modified accounts so that there are stored accounts
        // (note: the number of banks is arbitrary)
        for _ in 0..3 {
            let slot = bank.slot() + 1;
            bank =
                new_bank_from_parent_with_bank_forks(&bank_forks, bank, &Pubkey::default(), slot);
            bank.register_unique_recent_blockhash_for_test();
            bank.transfer(amount, &mint_keypair, &pubkey::new_rand())
                .unwrap();
            bank.fill_bank_with_ticks_for_tests();
            bank.squash();
            bank.force_flush_accounts_cache();
        }

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
        let roundtrip_bank = snapshot_bank_utils::bank_from_snapshot_archives(
            &[accounts_dir],
            &bank_snapshots_dir,
            &snapshot,
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

        assert_eq!(roundtrip_bank, *bank);
    }
}
