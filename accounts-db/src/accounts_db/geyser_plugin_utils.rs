use {
    crate::{
        accounts_db::{AccountStorageEntry, AccountsDb},
        accounts_update_notifier_interface::AccountsUpdateNotifierInterface,
    },
    solana_account::AccountSharedData,
    solana_clock::Slot,
    solana_measure::meas_dur,
    solana_metrics::*,
    solana_pubkey::Pubkey,
    solana_transaction::sanitized::SanitizedTransaction,
    std::{
        cmp::Reverse,
        ops::AddAssign,
        time::{Duration, Instant},
    },
};

#[derive(Default)]
pub struct GeyserPluginNotifyAtSnapshotRestoreStats {
    pub notified_accounts: usize,
    pub elapsed_notifying: Duration,
    pub total_pure_notify: Duration,
}

impl GeyserPluginNotifyAtSnapshotRestoreStats {
    pub fn report(&self) {
        datapoint_info!(
            "accountsdb_plugin_notify_account_restore_from_snapshot_summary",
            ("notified_accounts", self.notified_accounts, i64),
            (
                "elapsed_notifying_us",
                self.elapsed_notifying.as_micros(),
                i64
            ),
            (
                "total_pure_notify_us",
                self.total_pure_notify.as_micros(),
                i64
            ),
        );
    }
}

impl AddAssign for GeyserPluginNotifyAtSnapshotRestoreStats {
    fn add_assign(&mut self, other: Self) {
        self.notified_accounts += other.notified_accounts;
        self.elapsed_notifying += other.elapsed_notifying;
        self.total_pure_notify += other.total_pure_notify;
    }
}

impl AccountsDb {
    /// Notify the plugins of account data when AccountsDb is restored from a snapshot.
    ///
    /// Since accounts may have multiple versions in different slots, plugins must handle
    /// deduplication by inspected the slot and write version of each account notification.
    pub fn notify_account_restore_from_snapshot(&self) {
        let Some(accounts_update_notifier) = &self.accounts_update_notifier else {
            return;
        };

        let mut notify_stats = GeyserPluginNotifyAtSnapshotRestoreStats::default();
        if accounts_update_notifier.snapshot_notifications_enabled() {
            let mut slots = self.storage.all_slots();
            slots.sort_unstable_by_key(|&slot| Reverse(slot));
            slots
                .into_iter()
                .filter_map(|slot| self.storage.get_slot_storage_entry(slot))
                .map(|storage| {
                    Self::notify_accounts_in_storage(accounts_update_notifier.as_ref(), &storage)
                })
                .for_each(|stats| notify_stats += stats);
        }

        accounts_update_notifier.notify_end_of_restore_from_snapshot();
        notify_stats.report();
    }

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

    fn notify_accounts_in_storage(
        notifier: &dyn AccountsUpdateNotifierInterface,
        storage: &AccountStorageEntry,
    ) -> GeyserPluginNotifyAtSnapshotRestoreStats {
        let mut pure_notify_time = Duration::ZERO;
        let mut i = 0;
        let notifying_start = Instant::now();
        storage.accounts.scan_accounts_for_geyser(|account| {
            i += 1;
            // later entries in the same slot are more recent and override earlier accounts for the same pubkey
            // We can pass an incrementing number here for write_version in the future, if the storage does not have a write_version.
            // As long as all accounts for this slot are in 1 append vec that can be iterated oldest to newest.
            let (_, notify_dur) = meas_dur!(notifier.notify_account_restore_from_snapshot(
                storage.slot(),
                i as u64,
                &account
            ));
            pure_notify_time += notify_dur;
        });
        let notifying_time = notifying_start.elapsed();

        GeyserPluginNotifyAtSnapshotRestoreStats {
            notified_accounts: i,
            elapsed_notifying: notifying_time,
            total_pure_notify: pure_notify_time,
        }
    }
}

#[cfg(test)]
pub mod tests {
    use {
        super::*,
        crate::accounts_update_notifier_interface::{
            AccountForGeyser, AccountsUpdateNotifier, AccountsUpdateNotifierInterface,
        },
        dashmap::DashMap,
        solana_account::ReadableAccount as _,
        std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
    };

    impl AccountsDb {
        pub fn set_geyser_plugin_notifer(&mut self, notifier: Option<AccountsUpdateNotifier>) {
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
                .push((slot, write_version, account.to_account_shared_data()));
        }

        fn notify_end_of_restore_from_snapshot(&self) {
            self.is_startup_done.store(true, Ordering::Relaxed);
        }
    }

    #[test]
    fn test_notify_account_restore_from_snapshot() {
        let mut accounts = AccountsDb::new_single_for_tests();
        let key1 = Pubkey::new_unique();
        let key2 = Pubkey::new_unique();
        let key3 = Pubkey::new_unique();
        let account = AccountSharedData::new(1, 0, &Pubkey::default());

        // Account with key1 is updated twice in two different slots, should get notified twice
        accounts.store_uncached(0, &[(&key1, &account)]);
        accounts.store_uncached(1, &[(&key1, &account)]);

        // Account with key2 is updated in a single slot, should get notified once
        accounts.store_uncached(2, &[(&key2, &account)]);

        // Account with key3 is updated twice in a single slot, should get notified twice
        accounts.store_uncached(3, &[(&key3, &account)]);
        accounts.store_uncached(3, &[(&key3, &account)]);

        // Do the notification
        let notifier = GeyserTestPlugin::default();
        let notifier = Arc::new(notifier);
        accounts.set_geyser_plugin_notifer(Some(notifier.clone()));
        accounts.notify_account_restore_from_snapshot();

        // Ensure key1 was notified twice in different slots
        {
            let notified_key1 = notifier.accounts_notified.get(&key1).unwrap();
            assert_eq!(notified_key1.len(), 2);
            let (slot, write_version, _account) = &notified_key1[0];
            assert_eq!(*slot, 1);
            assert_eq!(*write_version, 1);
            let (slot, write_version, _account) = &notified_key1[1];
            assert_eq!(*slot, 0);
            assert_eq!(*write_version, 1);
        }

        // Ensure key2 was notified once
        {
            let notified_key2 = notifier.accounts_notified.get(&key2).unwrap();
            assert_eq!(notified_key2.len(), 1);
            let (slot, write_version, _account) = &notified_key2[0];
            assert_eq!(*slot, 2);
            assert_eq!(*write_version, 1);
        }

        // Ensure key3 was notified twice in the same slot
        {
            let notified_key3 = notifier.accounts_notified.get(&key3).unwrap();
            assert_eq!(notified_key3.len(), 2);
            let (slot, write_version, _account) = &notified_key3[0];
            assert_eq!(*slot, 3);
            assert_eq!(*write_version, 1);
            let (slot, write_version, _account) = &notified_key3[1];
            assert_eq!(*slot, 3);
            assert_eq!(*write_version, 2);
        }

        // Ensure we were notified that startup is done
        assert!(notifier.is_startup_done.load(Ordering::Relaxed));
    }

    #[test]
    fn test_notify_account_at_accounts_update() {
        let mut accounts = AccountsDb::new_single_for_tests();

        let notifier = GeyserTestPlugin::default();

        let notifier = Arc::new(notifier);
        accounts.set_geyser_plugin_notifer(Some(notifier.clone()));

        // Account with key1 is updated twice in two different slots -- should only get notified twice.
        // Account with key2 is updated slot0, should get notified once
        // Account with key3 is updated in slot1, should get notified once
        let key1 = solana_pubkey::new_rand();
        let account1_lamports1: u64 = 1;
        let account1 =
            AccountSharedData::new(account1_lamports1, 1, AccountSharedData::default().owner());
        let slot0 = 0;
        accounts.store_cached((slot0, &[(&key1, &account1)][..]), None);

        let key2 = solana_pubkey::new_rand();
        let account2_lamports: u64 = 200;
        let account2 =
            AccountSharedData::new(account2_lamports, 1, AccountSharedData::default().owner());
        accounts.store_cached((slot0, &[(&key2, &account2)][..]), None);

        let account1_lamports2 = 2;
        let slot1 = 1;
        let account1 = AccountSharedData::new(account1_lamports2, 1, account1.owner());
        accounts.store_cached((slot1, &[(&key1, &account1)][..]), None);

        let key3 = solana_pubkey::new_rand();
        let account3_lamports: u64 = 300;
        let account3 =
            AccountSharedData::new(account3_lamports, 1, AccountSharedData::default().owner());
        accounts.store_cached((slot1, &[(&key3, &account3)][..]), None);

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
