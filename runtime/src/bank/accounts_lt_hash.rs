use {
    super::Bank,
    solana_accounts_db::accounts_db::AccountsDb,
    solana_lattice_hash::lt_hash::{Checksum as LtChecksum, LtHash},
    solana_measure::{meas_dur, measure::Measure},
    solana_sdk::{
        account::{accounts_equal, AccountSharedData},
        pubkey::Pubkey,
    },
    solana_svm::transaction_processing_callback::AccountState,
    std::time::Duration,
};

impl Bank {
    /// Returns if the accounts lt hash is enabled
    pub fn is_accounts_lt_hash_enabled(&self) -> bool {
        self.rc
            .accounts
            .accounts_db
            .is_experimental_accumulator_hash_enabled()
    }

    /// Updates the accounts lt hash
    ///
    /// When freezing a bank, we compute and update the accounts lt hash.
    /// For each account modified in this bank, we:
    /// - mix out its previous state, and
    /// - mix in its current state
    ///
    /// Since this function is non-idempotent, it should only be called once per bank.
    pub fn update_accounts_lt_hash(&self) -> LtChecksum {
        debug_assert!(self.is_accounts_lt_hash_enabled());
        let delta_lt_hash = self.calculate_delta_lt_hash();
        let mut accounts_lt_hash = self.accounts_lt_hash.lock().unwrap();
        accounts_lt_hash.0.mix_in(&delta_lt_hash);
        accounts_lt_hash.0.checksum()
    }

    /// Calculates the lt hash *of only this slot*
    ///
    /// This can be thought of as akin to the accounts delta hash.
    ///
    /// For each account modified in this bank, we:
    /// - mix out its previous state, and
    /// - mix in its current state
    ///
    /// This function is idempotent, and may be called more than once.
    fn calculate_delta_lt_hash(&self) -> LtHash {
        debug_assert!(self.is_accounts_lt_hash_enabled());
        let measure_total = Measure::start("");
        let slot = self.slot();

        // If we don't find the account in the cache, we need to go load it.
        // We want the version of the account *before* it was written in this slot.
        // Bank::ancestors *includes* this slot, so we need to remove it before loading.
        let strictly_ancestors = {
            let mut ancestors = self.ancestors.clone();
            ancestors.remove(&self.slot());
            ancestors
        };

        // Get all the accounts stored in this slot.
        // Since this bank is in the middle of being frozen, it hasn't been rooted.
        // That means the accounts should all be in the write cache, and loading will be fast.
        let (accounts_curr, loading_accounts_curr_time) = meas_dur!({
            self.rc
                .accounts
                .accounts_db
                .get_pubkey_hash_account_for_slot(slot)
        });
        let num_accounts_total = accounts_curr.len();

        let mut num_accounts_unmodified = 0_usize;
        let mut num_cache_misses = 0_usize;
        let mut loading_accounts_prev_time = Duration::default();
        let mut computing_hashes_time = Duration::default();
        let mut mixing_hashes_time = Duration::default();
        let mut delta_lt_hash = LtHash::identity();
        let cache_for_accounts_lt_hash = self.cache_for_accounts_lt_hash.read().unwrap();
        for curr in accounts_curr {
            let pubkey = &curr.pubkey;

            // load the initial state of the account
            let (initial_state_of_account, measure_load) = meas_dur!({
                match cache_for_accounts_lt_hash.get(pubkey) {
                    Some(initial_state_of_account) => initial_state_of_account.clone(),
                    None => {
                        num_cache_misses += 1;
                        // If the initial state of the account is not in the accounts lt hash cache,
                        // it is likely this account was stored *outside* of transaction processing
                        // (e.g. as part of rent collection).  Do not populate the read cache,
                        // as this account likely will not be accessed again soon.
                        let account_slot = self
                            .rc
                            .accounts
                            .load_with_fixed_root_do_not_populate_read_cache(
                                &strictly_ancestors,
                                pubkey,
                            );
                        match account_slot {
                            Some((account, _slot)) => InitialStateOfAccount::Alive(account),
                            None => InitialStateOfAccount::Dead,
                        }
                    }
                }
            });
            loading_accounts_prev_time += measure_load;

            // mix out the previous version of the account
            match initial_state_of_account {
                InitialStateOfAccount::Dead => {
                    // nothing to do here
                }
                InitialStateOfAccount::Alive(prev_account) => {
                    if accounts_equal(&curr.account, &prev_account) {
                        // this account didn't actually change, so can skip it for lt hashing
                        num_accounts_unmodified += 1;
                        continue;
                    }
                    let (prev_lt_hash, measure_hashing) =
                        meas_dur!(AccountsDb::lt_hash_account(&prev_account, pubkey));
                    let (_, measure_mixing) = meas_dur!(delta_lt_hash.mix_out(&prev_lt_hash.0));
                    computing_hashes_time += measure_hashing;
                    mixing_hashes_time += measure_mixing;
                }
            }

            // mix in the new version of the account
            let (curr_lt_hash, measure_hashing) =
                meas_dur!(AccountsDb::lt_hash_account(&curr.account, pubkey));
            let (_, measure_mixing) = meas_dur!(delta_lt_hash.mix_in(&curr_lt_hash.0));
            computing_hashes_time += measure_hashing;
            mixing_hashes_time += measure_mixing;
        }
        drop(cache_for_accounts_lt_hash);

        let total_time = measure_total.end_as_duration();
        let num_accounts_modified = num_accounts_total.saturating_sub(num_accounts_unmodified);
        datapoint_info!(
            "bank-accounts_lt_hash",
            ("slot", slot, i64),
            ("num_accounts_total", num_accounts_total, i64),
            ("num_accounts_modified", num_accounts_modified, i64),
            ("num_accounts_unmodified", num_accounts_unmodified, i64),
            ("num_cache_misses", num_cache_misses, i64),
            ("total_us", total_time.as_micros(), i64),
            (
                "loading_accounts_prev_us",
                loading_accounts_prev_time.as_micros(),
                i64
            ),
            (
                "loading_accounts_curr_us",
                loading_accounts_curr_time.as_micros(),
                i64
            ),
            (
                "computing_hashes_us",
                computing_hashes_time.as_micros(),
                i64
            ),
            ("mixing_hashes_us", mixing_hashes_time.as_micros(), i64),
        );

        delta_lt_hash
    }

    /// Caches initial state of writeable accounts
    ///
    /// If a transaction account is writeable, cache its initial account state.
    /// The initial state is needed when computing the accounts lt hash for the slot, and caching
    /// the initial state saves us from having to look it up on disk later.
    pub fn inspect_account_for_accounts_lt_hash(
        &self,
        address: &Pubkey,
        account_state: &AccountState,
        is_writable: bool,
    ) {
        debug_assert!(self.is_accounts_lt_hash_enabled());
        if !is_writable {
            // if the account is not writable, then it cannot be modified; nothing to do here
            return;
        }

        // Only insert the account the *first* time we see it.
        // We want to capture the value of the account *before* any modifications during this slot.
        let is_in_cache = self
            .cache_for_accounts_lt_hash
            .read()
            .unwrap()
            .contains_key(address);
        if !is_in_cache {
            self.cache_for_accounts_lt_hash
                .write()
                .unwrap()
                .entry(*address)
                .or_insert_with(|| match account_state {
                    AccountState::Dead => InitialStateOfAccount::Dead,
                    AccountState::Alive(account) => {
                        InitialStateOfAccount::Alive((*account).clone())
                    }
                });
        }
    }
}

/// The initial state of an account prior to being modified in this slot/transaction
#[derive(Debug, Clone)]
pub enum InitialStateOfAccount {
    /// The account was initiall dead
    Dead,
    /// The account was initially alive
    Alive(AccountSharedData),
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::bank::tests::new_bank_from_parent_with_bank_forks,
        solana_accounts_db::accounts::Accounts,
        solana_sdk::{
            account::{ReadableAccount as _, WritableAccount as _},
            fee_calculator::FeeRateGovernor,
            genesis_config::create_genesis_config,
            native_token::LAMPORTS_PER_SOL,
            pubkey::{self, Pubkey},
            signature::Signer as _,
            signer::keypair::Keypair,
        },
        std::{cmp, str::FromStr as _, sync::Arc},
    };

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
            create_genesis_config(123_456_789 * LAMPORTS_PER_SOL);
        genesis_config.fee_rate_governor = FeeRateGovernor::new(0, 0);
        let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        bank.rc
            .accounts
            .accounts_db
            .set_is_experimental_accumulator_hash_enabled(true);

        // ensure the accounts lt hash is enabled, otherwise this test doesn't actually do anything...
        assert!(bank.is_accounts_lt_hash_enabled());

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

        // manually freeze the bank to trigger update_accounts_lt_hash() to run
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

        // These sysvars are also updated, but outside of transaction processing.  This means they
        // will not be in the accounts lt hash cache, but *will* be in the list of modified
        // accounts.  They must be included in the accounts lt hash.
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

        // send lamports to account 4, then turn around and send them to account 3
        // account 3 will be alive, and account 4 will end dead
        bank.register_unique_recent_blockhash_for_test();
        bank.transfer(amount, &mint_keypair, &keypair4.pubkey())
            .unwrap();
        bank.register_unique_recent_blockhash_for_test();
        bank.transfer(amount, &keypair4, &keypair3.pubkey())
            .unwrap();

        // store account 5 into this new bank, unchanged
        bank.rc.accounts.store_cached(
            (
                bank.slot(),
                [(&keypair5.pubkey(), &prev_account5.clone().unwrap())].as_slice(),
            ),
            None,
        );

        // freeze the bank to trigger update_accounts_lt_hash() to run
        bank.freeze();

        let actual_delta_lt_hash = bank.calculate_delta_lt_hash();
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

        let mut expected_delta_lt_hash = LtHash::identity();
        let mut expected_accounts_lt_hash = prev_accounts_lt_hash.clone();
        let mut updater =
            |address: &Pubkey, prev: Option<AccountSharedData>, post: Option<AccountSharedData>| {
                // if there was an alive account, mix out
                if let Some(prev) = prev {
                    let prev_lt_hash = AccountsDb::lt_hash_account(&prev, address);
                    expected_delta_lt_hash.mix_out(&prev_lt_hash.0);
                    expected_accounts_lt_hash.0.mix_out(&prev_lt_hash.0);
                }

                // mix in the new one
                let post = post.unwrap_or_default();
                let post_lt_hash = AccountsDb::lt_hash_account(&post, address);
                expected_delta_lt_hash.mix_in(&post_lt_hash.0);
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

        // now make sure the delta lt hashes match
        let expected = expected_delta_lt_hash.checksum();
        let actual = actual_delta_lt_hash.checksum();
        assert_eq!(
            expected, actual,
            "delta_lt_hash, expected: {expected}, actual: {actual}",
        );

        // ...and the accounts lt hashes match too
        let expected = expected_accounts_lt_hash.0.checksum();
        let actual = post_accounts_lt_hash.0.checksum();
        assert_eq!(
            expected, actual,
            "accounts_lt_hash, expected: {expected}, actual: {actual}",
        );
    }

    #[test]
    fn test_inspect_account_for_accounts_lt_hash() {
        let accounts_db = AccountsDb::default_for_tests();
        accounts_db.set_is_experimental_accumulator_hash_enabled(true);
        let accounts = Accounts::new(Arc::new(accounts_db));
        let bank = Bank::default_with_accounts(accounts);

        // ensure the accounts lt hash is enabled, otherwise this test doesn't actually do anything...
        assert!(bank.is_accounts_lt_hash_enabled());

        // the cache should start off empty
        assert_eq!(bank.cache_for_accounts_lt_hash.read().unwrap().len(), 0);

        // ensure non-writable accounts are *not* added to the cache
        bank.inspect_account_for_accounts_lt_hash(
            &Pubkey::new_unique(),
            &AccountState::Dead,
            false,
        );
        bank.inspect_account_for_accounts_lt_hash(
            &Pubkey::new_unique(),
            &AccountState::Alive(&AccountSharedData::default()),
            false,
        );
        assert_eq!(bank.cache_for_accounts_lt_hash.read().unwrap().len(), 0);

        // ensure *new* accounts are added to the cache
        let address = Pubkey::new_unique();
        bank.inspect_account_for_accounts_lt_hash(&address, &AccountState::Dead, true);
        assert_eq!(bank.cache_for_accounts_lt_hash.read().unwrap().len(), 1);
        assert!(bank
            .cache_for_accounts_lt_hash
            .read()
            .unwrap()
            .contains_key(&address));

        // ensure *existing* accounts are added to the cache
        let address = Pubkey::new_unique();
        let initial_lamports = 123;
        let mut account = AccountSharedData::new(initial_lamports, 0, &Pubkey::default());
        bank.inspect_account_for_accounts_lt_hash(&address, &AccountState::Alive(&account), true);
        assert_eq!(bank.cache_for_accounts_lt_hash.read().unwrap().len(), 2);
        if let InitialStateOfAccount::Alive(cached_account) = bank
            .cache_for_accounts_lt_hash
            .read()
            .unwrap()
            .get(&address)
            .unwrap()
        {
            assert_eq!(*cached_account, account);
        } else {
            panic!("wrong initial state for account");
        };

        // ensure if an account is modified multiple times that we only cache the *first* one
        let updated_lamports = account.lamports() + 1;
        account.set_lamports(updated_lamports);
        bank.inspect_account_for_accounts_lt_hash(&address, &AccountState::Alive(&account), true);
        assert_eq!(bank.cache_for_accounts_lt_hash.read().unwrap().len(), 2);
        if let InitialStateOfAccount::Alive(cached_account) = bank
            .cache_for_accounts_lt_hash
            .read()
            .unwrap()
            .get(&address)
            .unwrap()
        {
            assert_eq!(cached_account.lamports(), initial_lamports);
        } else {
            panic!("wrong initial state for account");
        };

        // and ensure multiple updates are handled correctly when the account is initially dead
        {
            let address = Pubkey::new_unique();
            bank.inspect_account_for_accounts_lt_hash(&address, &AccountState::Dead, true);
            assert_eq!(bank.cache_for_accounts_lt_hash.read().unwrap().len(), 3);
            match bank
                .cache_for_accounts_lt_hash
                .read()
                .unwrap()
                .get(&address)
                .unwrap()
            {
                InitialStateOfAccount::Dead => { /* this is expected, nothing to do here*/ }
                _ => panic!("wrong initial state for account"),
            };

            bank.inspect_account_for_accounts_lt_hash(
                &address,
                &AccountState::Alive(&AccountSharedData::default()),
                true,
            );
            assert_eq!(bank.cache_for_accounts_lt_hash.read().unwrap().len(), 3);
            match bank
                .cache_for_accounts_lt_hash
                .read()
                .unwrap()
                .get(&address)
                .unwrap()
            {
                InitialStateOfAccount::Dead => { /* this is expected, nothing to do here*/ }
                _ => panic!("wrong initial state for account"),
            };
        }
    }

    #[test]
    fn test_calculate_accounts_lt_hash_at_startup() {
        let (genesis_config, mint_keypair) = create_genesis_config(123_456_789 * LAMPORTS_PER_SOL);
        let (mut bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        bank.rc
            .accounts
            .accounts_db
            .set_is_experimental_accumulator_hash_enabled(true);

        // ensure the accounts lt hash is enabled, otherwise this test doesn't actually do anything...
        assert!(bank.is_accounts_lt_hash_enabled());

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
            .calculate_accounts_lt_hash_at_startup(&bank.ancestors, bank.slot());

        let expected = expected_accounts_lt_hash.0.checksum();
        let actual = calculated_accounts_lt_hash.0.checksum();
        assert_eq!(expected, actual, "expected: {expected}, actual: {actual}");
    }
}
