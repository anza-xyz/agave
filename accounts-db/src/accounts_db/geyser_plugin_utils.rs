use {
    crate::accounts_db::AccountsDb, solana_account::AccountSharedData, solana_clock::Slot,
    solana_pubkey::Pubkey, solana_transaction::sanitized::SanitizedTransaction,
};

impl AccountsDb {
    pub fn notify_account_at_accounts_update(
        &self,
        slot: Slot,
        account: &AccountSharedData,
        txn: &Option<&SanitizedTransaction>,
        pubkey: &Pubkey,
        write_version: u64,
    ) {
        if let Some(accounts_update_notifier) = &self.accounts_update_notifier {
            accounts_update_notifier.notify_account_update(
                slot,
                account,
                txn,
                pubkey,
                write_version,
            );
        }
    }
}

#[cfg(test)]
pub mod tests {
    use {
        super::*,
        crate::{
            accounts_db::{AccountsDbConfig, MarkObsoleteAccounts, ACCOUNTS_DB_CONFIG_FOR_TESTING},
            accounts_update_notifier_interface::{
                AccountForGeyser, AccountsUpdateNotifier, AccountsUpdateNotifierInterface,
            },
            utils::create_account_shared_data,
        },
        dashmap::DashMap,
        solana_account::ReadableAccount as _,
        std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        test_case::test_case,
    };

    impl AccountsDb {
        pub fn set_geyser_plugin_notifier(&mut self, notifier: Option<AccountsUpdateNotifier>) {
            self.accounts_update_notifier = notifier;
        }
    }

    #[derive(Debug, Default)]
    struct GeyserTestPlugin {
        pub accounts_notified: DashMap<Pubkey, Vec<(Slot, u64, AccountSharedData)>>,
        pub is_startup_done: AtomicBool,
    }

    impl AccountsUpdateNotifierInterface for GeyserTestPlugin {
        fn snapshot_notifications_enabled(&self) -> bool {
            true
        }

        /// Notified when an account is updated at runtime, due to transaction activities
        fn notify_account_update(
            &self,
            slot: Slot,
            account: &AccountSharedData,
            _txn: &Option<&SanitizedTransaction>,
            pubkey: &Pubkey,
            write_version: u64,
        ) {
            self.accounts_notified.entry(*pubkey).or_default().push((
                slot,
                write_version,
                account.clone(),
            ));
        }

        /// Notified when the AccountsDb is initialized at start when restored
        /// from a snapshot.
        fn notify_account_restore_from_snapshot(
            &self,
            slot: Slot,
            write_version: u64,
            account: &AccountForGeyser<'_>,
        ) {
            self.accounts_notified
                .entry(*account.pubkey)
                .or_default()
                .push((slot, write_version, create_account_shared_data(account)));
        }

        fn notify_end_of_restore_from_snapshot(&self) {
            self.is_startup_done.store(true, Ordering::Relaxed);
        }
    }

    #[test_case(MarkObsoleteAccounts::Enabled)]
    #[test_case(MarkObsoleteAccounts::Disabled)]
    fn test_notify_account_restore_from_snapshot(mark_obsolete_accounts: MarkObsoleteAccounts) {
        let mut accounts_db = AccountsDb::new_with_config(
            Vec::new(),
            AccountsDbConfig {
                mark_obsolete_accounts,
                ..ACCOUNTS_DB_CONFIG_FOR_TESTING
            },
            None,
            Arc::default(),
        );
        let key1 = Pubkey::new_unique();
        let key2 = Pubkey::new_unique();
        let account = AccountSharedData::new(1, 0, &Pubkey::default());

        // Account with key1 is updated twice in two different slots, should get notified twice
        // Need to add root and flush write cache for each slot to ensure accounts are written
        // to correct slots. Cache flush can skip writes if accounts have already been written to
        // a newer slot
        let slot0 = 0;
        let storage0 = accounts_db.create_and_insert_store(slot0, /*size*/ 4_096, "");
        storage0
            .accounts
            .write_accounts(&(slot0, [(&key1, &account)].as_slice()), /*skip*/ 0);

        let slot1 = 1;
        let storage1 = accounts_db.create_and_insert_store(slot1, /*size*/ 4_096, "");
        storage1
            .accounts
            .write_accounts(&(slot1, [(&key1, &account)].as_slice()), /*skip*/ 0);

        // Account with key2 is updated in a single slot, should get notified once
        let slot2 = 2;
        let storage2 = accounts_db.create_and_insert_store(slot2, /*size*/ 4_096, "");
        storage2
            .accounts
            .write_accounts(&(slot2, [(&key2, &account)].as_slice()), /*skip*/ 0);

        // Do the notification
        let notifier = GeyserTestPlugin::default();
        let notifier = Arc::new(notifier);
        accounts_db.set_geyser_plugin_notifier(Some(notifier.clone()));
        accounts_db.generate_index(None, false);

        // Ensure key1 was notified twice in different slots
        {
            let notified_key1 = notifier.accounts_notified.get(&key1).unwrap();
            assert_eq!(notified_key1.len(), 2);

            // Since index generation goes through storages in parallel, there's not a
            // deterministic order for which slots will notify first.
            // So, we sort the accounts_notified values to ensure we can assert correctly.
            let mut notified_key1_values = notified_key1.value().clone();
            notified_key1_values.sort_unstable_by_key(|k| k.0);

            let (slot, write_version, _account) = &notified_key1_values[0];
            assert_eq!(*slot, slot0);
            assert_eq!(*write_version, 0);
            let (slot, write_version, _account) = &notified_key1_values[1];
            assert_eq!(*slot, slot1);
            assert_eq!(*write_version, 0);
        }

        // Ensure key2 was notified once
        {
            let notified_key2 = notifier.accounts_notified.get(&key2).unwrap();
            assert_eq!(notified_key2.len(), 1);
            let (slot, write_version, _account) = &notified_key2[0];
            assert_eq!(*slot, slot2);
            assert_eq!(*write_version, 0);
        }

        // Ensure we were notified that startup is done
        assert!(notifier.is_startup_done.load(Ordering::Relaxed));
    }

    #[test]
    fn test_notify_account_at_accounts_update() {
        let mut accounts = AccountsDb::new_single_for_tests();

        let notifier = GeyserTestPlugin::default();

        let notifier = Arc::new(notifier);
        accounts.set_geyser_plugin_notifier(Some(notifier.clone()));

        // Account with key1 is updated twice in two different slots -- should only get notified twice.
        // Account with key2 is updated slot0, should get notified once
        // Account with key3 is updated in slot1, should get notified once
        let key1 = solana_pubkey::new_rand();
        let account1_lamports1: u64 = 1;
        let account1 =
            AccountSharedData::new(account1_lamports1, 1, AccountSharedData::default().owner());
        let slot0 = 0;
        accounts.store_for_tests((slot0, &[(&key1, &account1)][..]));

        let key2 = solana_pubkey::new_rand();
        let account2_lamports: u64 = 200;
        let account2 =
            AccountSharedData::new(account2_lamports, 1, AccountSharedData::default().owner());
        accounts.store_for_tests((slot0, &[(&key2, &account2)][..]));

        let account1_lamports2 = 2;
        let slot1 = 1;
        let account1 = AccountSharedData::new(account1_lamports2, 1, account1.owner());
        accounts.store_for_tests((slot1, &[(&key1, &account1)][..]));

        let key3 = solana_pubkey::new_rand();
        let account3_lamports: u64 = 300;
        let account3 =
            AccountSharedData::new(account3_lamports, 1, AccountSharedData::default().owner());
        accounts.store_for_tests((slot1, &[(&key3, &account3)][..]));

        assert_eq!(notifier.accounts_notified.get(&key1).unwrap().len(), 2);
        assert_eq!(
            notifier.accounts_notified.get(&key1).unwrap()[0]
                .2
                .lamports(),
            account1_lamports1
        );
        assert_eq!(notifier.accounts_notified.get(&key1).unwrap()[0].0, slot0);
        assert_eq!(
            notifier.accounts_notified.get(&key1).unwrap()[1]
                .2
                .lamports(),
            account1_lamports2
        );
        assert_eq!(notifier.accounts_notified.get(&key1).unwrap()[1].0, slot1);

        assert_eq!(notifier.accounts_notified.get(&key2).unwrap().len(), 1);
        assert_eq!(
            notifier.accounts_notified.get(&key2).unwrap()[0]
                .2
                .lamports(),
            account2_lamports
        );
        assert_eq!(notifier.accounts_notified.get(&key2).unwrap()[0].0, slot0);
        assert_eq!(notifier.accounts_notified.get(&key3).unwrap().len(), 1);
        assert_eq!(
            notifier.accounts_notified.get(&key3).unwrap()[0]
                .2
                .lamports(),
            account3_lamports
        );
        assert_eq!(notifier.accounts_notified.get(&key3).unwrap()[0].0, slot1);
    }
}
