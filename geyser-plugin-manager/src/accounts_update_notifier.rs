/// Module responsible for notifying plugins of account updates
use {
    agave_geyser_plugin_host::GeyserPluginHost,
    agave_geyser_plugin_interface::geyser_plugin_interface::{
        ReplicaAccountInfoV3, ReplicaAccountInfoVersions,
    },
    arc_swap::ArcSwap,
    log::*,
    solana_account::{AccountSharedData, ReadableAccount},
    solana_accounts_db::accounts_update_notifier_interface::{
        AccountForGeyser, AccountsUpdateNotifierInterface,
    },
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    solana_runtime::bank::BankId,
    solana_transaction::sanitized::SanitizedTransaction,
    std::sync::Arc,
};
#[derive(Debug)]
pub(crate) struct AccountsUpdateNotifierImpl {
    plugin_manager: Arc<ArcSwap<GeyserPluginHost>>,
    account_notifications_enabled: bool,
    snapshot_notifications_enabled: bool,
}

impl AccountsUpdateNotifierInterface for AccountsUpdateNotifierImpl {
    fn account_notifications_enabled(&self) -> bool {
        self.account_notifications_enabled
    }

    fn notify_bank_creation(&self, slot: Slot, parent: Option<Slot>, bank_id: BankId) {
        let plugin_manager = self.plugin_manager.load();
        for plugin in plugin_manager.plugins() {
            if let Err(err) = plugin.notify_bank_creation(slot, parent, bank_id.into()) {
                error!(
                    "Failed to notify bank creation at slot {slot}, error: {err} to plugin {}",
                    plugin.name()
                );
            }
        }
    }

    fn snapshot_notifications_enabled(&self) -> bool {
        self.snapshot_notifications_enabled
    }

    fn notify_account_update(
        &self,
        slot: Slot,
        bank_id: BankId,
        account: &AccountSharedData,
        txn: &Option<&SanitizedTransaction>,
        pubkey: &Pubkey,
        write_version: u64,
    ) {
        let account_info =
            self.accountinfo_from_shared_account_data(account, txn, pubkey, write_version);
        self.notify_plugins_of_account_update_for_bank(account_info, slot, bank_id);
    }

    fn notify_account_restore_from_snapshot(
        &self,
        slot: Slot,
        write_version: u64,
        account: &AccountForGeyser<'_>,
    ) {
        let mut account = self.accountinfo_from_account_for_geyser(account);
        account.write_version = write_version;
        self.notify_plugins_of_account_update_from_snapshot(account, slot);
    }

    fn notify_end_of_restore_from_snapshot(&self) {
        let plugin_manager = self.plugin_manager.load();
        if plugin_manager.plugins().next().is_none() {
            return;
        }

        for plugin in plugin_manager.plugins() {
            match plugin.notify_end_of_startup() {
                Err(err) => {
                    error!(
                        "Failed to notify the end of restore from snapshot, error: {} to plugin {}",
                        err,
                        plugin.name()
                    )
                }
                Ok(_) => {
                    trace!(
                        "Successfully notified the end of restore from snapshot to plugin {}",
                        plugin.name()
                    );
                }
            }
        }
    }
}

impl AccountsUpdateNotifierImpl {
    pub fn new(
        plugin_manager: Arc<ArcSwap<GeyserPluginHost>>,
        always_enabled: bool,
    ) -> Option<Self> {
        let plugins = plugin_manager.load();
        if plugins.plugins().next().is_none() && !always_enabled {
            return None;
        }
        let account_notifications_enabled =
            plugins.account_data_notifications_enabled() || always_enabled;
        let snapshot_notifications_enabled =
            account_notifications_enabled && plugins.account_data_snapshot_notifications_enabled();
        drop(plugins);
        Some(Self {
            plugin_manager,
            account_notifications_enabled,
            snapshot_notifications_enabled,
        })
    }

    fn accountinfo_from_shared_account_data<'a>(
        &self,
        account: &'a AccountSharedData,
        txn: &'a Option<&'a SanitizedTransaction>,
        pubkey: &'a Pubkey,
        write_version: u64,
    ) -> ReplicaAccountInfoV3<'a> {
        ReplicaAccountInfoV3 {
            pubkey: pubkey.as_ref(),
            lamports: account.lamports(),
            owner: account.owner().as_ref(),
            executable: account.executable(),
            rent_epoch: account.rent_epoch(),
            data: account.data(),
            write_version,
            txn: *txn,
        }
    }

    fn accountinfo_from_account_for_geyser<'a>(
        &self,
        account: &'a AccountForGeyser<'_>,
    ) -> ReplicaAccountInfoV3<'a> {
        ReplicaAccountInfoV3 {
            pubkey: account.pubkey.as_ref(),
            lamports: account.lamports(),
            owner: account.owner().as_ref(),
            executable: account.executable(),
            rent_epoch: account.rent_epoch(),
            data: account.data(),
            write_version: 0, // can/will be populated afterwards
            txn: None,
        }
    }

    fn notify_plugins_of_account_update_from_snapshot(
        &self,
        account: ReplicaAccountInfoV3,
        slot: Slot,
    ) {
        let plugin_manager = self.plugin_manager.load();

        if plugin_manager.plugins().next().is_none() {
            return;
        }
        for plugin in plugin_manager.plugins() {
            if !plugin.account_data_notifications_enabled() {
                continue;
            }
            match plugin
                .update_account_from_snapshot(ReplicaAccountInfoVersions::V0_0_3(&account), slot)
            {
                Err(err) => {
                    error!(
                        "Failed to update account {} at slot {}, error: {} to plugin {}",
                        bs58::encode(account.pubkey).into_string(),
                        slot,
                        err,
                        plugin.name()
                    )
                }
                Ok(_) => {
                    trace!(
                        "Successfully updated account {} at slot {} to plugin {}",
                        bs58::encode(account.pubkey).into_string(),
                        slot,
                        plugin.name()
                    );
                }
            }
        }
    }

    fn notify_plugins_of_account_update_for_bank(
        &self,
        account: ReplicaAccountInfoV3,
        slot: Slot,
        bank_id: BankId,
    ) {
        let plugin_manager = self.plugin_manager.load();

        if plugin_manager.plugins().next().is_none() {
            return;
        }
        for plugin in plugin_manager.plugins() {
            if !plugin.account_data_notifications_enabled() {
                continue;
            }
            match plugin.update_account_for_bank(
                ReplicaAccountInfoVersions::V0_0_3(&account),
                slot,
                bank_id.into(),
            ) {
                Err(err) => {
                    error!(
                        "Failed to update account {} at slot {}, error: {} to plugin {}",
                        bs58::encode(account.pubkey).into_string(),
                        slot,
                        err,
                        plugin.name()
                    )
                }
                Ok(_) => {
                    trace!(
                        "Successfully updated account {} at slot {} to plugin {}",
                        bs58::encode(account.pubkey).into_string(),
                        slot,
                        plugin.name()
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        agave_geyser_plugin_host::{GeyserPluginHost, LoadedGeyserPlugin},
        agave_geyser_plugin_interface::geyser_plugin_interface::{
            GeyserPlugin, ReplicaAccountInfoVersions,
        },
        arc_swap::ArcSwap,
        libloading::Library,
        solana_accounts_db::accounts_update_notifier_interface::AccountsUpdateNotifierInterface,
        std::sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    #[derive(Debug)]
    struct TestAccountPlugin {
        name: &'static str,
        account_updates_enabled: bool,
        account_update_count: Arc<AtomicUsize>,
        account_update_bank_ids: Arc<Mutex<Vec<u64>>>,
    }

    impl GeyserPlugin for TestAccountPlugin {
        fn name(&self) -> &'static str {
            self.name
        }

        fn update_account_for_bank(
            &self,
            account: ReplicaAccountInfoVersions,
            _slot: Slot,
            bank_id: u64,
        ) -> agave_geyser_plugin_interface::geyser_plugin_interface::Result<()> {
            let ReplicaAccountInfoVersions::V0_0_3(_account) = account else {
                panic!("expected V0_0_3 account info");
            };
            self.account_update_bank_ids.lock().unwrap().push(bank_id);
            self.account_update_count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn update_account_from_snapshot(
            &self,
            account: ReplicaAccountInfoVersions,
            _slot: Slot,
        ) -> agave_geyser_plugin_interface::geyser_plugin_interface::Result<()> {
            let ReplicaAccountInfoVersions::V0_0_3(_account) = account else {
                panic!("expected V0_0_3 account info");
            };
            self.account_update_count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn account_data_notifications_enabled(&self) -> bool {
            self.account_updates_enabled
        }
    }

    fn loaded_test_plugin(plugin: impl GeyserPlugin) -> Arc<LoadedGeyserPlugin> {
        #[cfg(unix)]
        let library = libloading::os::unix::Library::this();
        #[cfg(windows)]
        let library = libloading::os::windows::Library::this().unwrap();

        // SAFETY: This plugin is defined in the current executable and starts no background work.
        Arc::new(unsafe { LoadedGeyserPlugin::new(Library::from(library), Box::new(plugin), None) })
    }

    #[test]
    fn test_notify_account_update_skips_plugins_with_account_notifications_disabled() {
        let enabled_count = Arc::new(AtomicUsize::new(0));
        let disabled_count = Arc::new(AtomicUsize::new(0));
        let enabled_bank_ids = Arc::new(Mutex::new(Vec::new()));
        let disabled_bank_ids = Arc::new(Mutex::new(Vec::new()));
        let plugin_manager = Arc::new(ArcSwap::from(Arc::new(GeyserPluginHost::from_plugins(
            vec![
                loaded_test_plugin(TestAccountPlugin {
                    name: "enabled",
                    account_updates_enabled: true,
                    account_update_count: enabled_count.clone(),
                    account_update_bank_ids: enabled_bank_ids.clone(),
                }),
                loaded_test_plugin(TestAccountPlugin {
                    name: "disabled",
                    account_updates_enabled: false,
                    account_update_count: disabled_count.clone(),
                    account_update_bank_ids: disabled_bank_ids.clone(),
                }),
            ],
        ))));
        let notifier = AccountsUpdateNotifierImpl::new(plugin_manager, false).unwrap();
        let account = AccountSharedData::new(1, 0, &Pubkey::new_unique());
        let pubkey = Pubkey::new_unique();
        let bank_id = BankId::new(9);

        notifier.notify_account_update(42, bank_id, &account, &None, &pubkey, 7);

        assert_eq!(enabled_count.load(Ordering::Relaxed), 1);
        assert_eq!(disabled_count.load(Ordering::Relaxed), 0);
        assert_eq!(*enabled_bank_ids.lock().unwrap(), vec![u64::from(bank_id)]);
        assert!(disabled_bank_ids.lock().unwrap().is_empty());
    }

    #[test]
    fn test_notify_account_restore_from_snapshot_has_no_bank_id() {
        let account_update_count = Arc::new(AtomicUsize::new(0));
        let account_update_bank_ids = Arc::new(Mutex::new(Vec::new()));
        let plugin_manager = Arc::new(ArcSwap::from(Arc::new(GeyserPluginHost::from_plugins(
            vec![loaded_test_plugin(TestAccountPlugin {
                name: "enabled",
                account_updates_enabled: true,
                account_update_count: account_update_count.clone(),
                account_update_bank_ids: account_update_bank_ids.clone(),
            })],
        ))));
        let notifier = AccountsUpdateNotifierImpl::new(plugin_manager, false).unwrap();
        let pubkey = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let data = [1, 2, 3];
        let account = AccountForGeyser {
            pubkey: &pubkey,
            lamports: 1,
            owner: &owner,
            executable: false,
            rent_epoch: 0,
            data: &data,
        };

        notifier.notify_account_restore_from_snapshot(42, 7, &account);

        assert_eq!(account_update_count.load(Ordering::Relaxed), 1);
        assert_eq!(*account_update_bank_ids.lock().unwrap(), Vec::<u64>::new());
    }

    #[test]
    fn test_notify_snapshot_completion_reaches_every_plugin() {
        #[derive(Debug)]
        struct CompletionPlugin(Arc<AtomicUsize>);

        impl GeyserPlugin for CompletionPlugin {
            fn name(&self) -> &'static str {
                "snapshot-completion"
            }

            fn notify_end_of_startup(
                &self,
            ) -> agave_geyser_plugin_interface::geyser_plugin_interface::Result<()> {
                self.0.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        }

        let counts = [Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0))];
        let manager = GeyserPluginHost::from_plugins(
            counts
                .iter()
                .map(|count| loaded_test_plugin(CompletionPlugin(Arc::clone(count))))
                .collect(),
        );
        let notifier =
            AccountsUpdateNotifierImpl::new(Arc::new(ArcSwap::from_pointee(manager)), false)
                .unwrap();
        notifier.notify_end_of_restore_from_snapshot();
        for count in counts {
            assert_eq!(count.load(Ordering::Relaxed), 1);
        }
    }

    type BankCreation = (Slot, Option<Slot>, u64);

    #[derive(Debug)]
    struct BankCreationPlugin {
        creations: Arc<Mutex<Vec<BankCreation>>>,
        fail: bool,
    }

    impl GeyserPlugin for BankCreationPlugin {
        fn name(&self) -> &'static str {
            if self.fail {
                "failing"
            } else {
                "transaction-only"
            }
        }

        fn account_data_notifications_enabled(&self) -> bool {
            false
        }

        fn transaction_notifications_enabled(&self) -> bool {
            true
        }

        fn notify_bank_creation(
            &self,
            slot: Slot,
            parent: Option<Slot>,
            bank_id: u64,
        ) -> agave_geyser_plugin_interface::geyser_plugin_interface::Result<()> {
            self.creations.lock().unwrap().push((slot, parent, bank_id));
            if self.fail {
                Err(agave_geyser_plugin_interface::geyser_plugin_interface::GeyserPluginError::Custom(
                    "test callback failure".into(),
                ))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn test_bank_creation_without_account_subscriptions() {
        agave_logger::setup();
        let creations = Arc::new(Mutex::new(Vec::new()));
        let manager = Arc::new(ArcSwap::from_pointee(GeyserPluginHost::from_plugins(
            [true, false]
                .into_iter()
                .map(|fail| {
                    loaded_test_plugin(BankCreationPlugin {
                        creations: Arc::clone(&creations),
                        fail,
                    })
                })
                .collect(),
        )));
        assert!(manager.load().transaction_notifications_enabled());
        let notifier = AccountsUpdateNotifierImpl::new(manager, false).unwrap();
        assert!(!notifier.account_notifications_enabled());
        assert!(!notifier.snapshot_notifications_enabled());
        let accounts_db = solana_accounts_db::accounts_db::AccountsDb::new_with_config(
            Vec::new(),
            solana_accounts_db::accounts_db::ACCOUNTS_DB_CONFIG_FOR_TESTING,
            Some(Arc::new(notifier)),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        assert!(!accounts_db.has_accounts_update_notifier());
        accounts_db.notify_bank_creation(42, Some(40), BankId::new(7));
        // Failure in the first plugin must not prevent delivery to the second.
        assert_eq!(*creations.lock().unwrap(), [(42, Some(40), 7); 2]);
    }

    #[test]
    fn test_bank_creation_after_dynamic_plugin_load() {
        let manager = Arc::new(ArcSwap::from_pointee(GeyserPluginHost::default()));
        assert!(AccountsUpdateNotifierImpl::new(Arc::clone(&manager), false).is_none());
        let notifier = AccountsUpdateNotifierImpl::new(Arc::clone(&manager), true).unwrap();
        assert!(notifier.account_notifications_enabled());
        notifier.notify_bank_creation(0, None, BankId::new(0));
        let creations = Arc::new(Mutex::new(Vec::new()));
        manager.store(Arc::new(GeyserPluginHost::from_plugins(vec![
            loaded_test_plugin(BankCreationPlugin {
                creations: Arc::clone(&creations),
                fail: false,
            }),
        ])));
        notifier.notify_bank_creation(1, Some(0), BankId::new(1));
        assert_eq!(*creations.lock().unwrap(), [(1, Some(0), 1)]);
    }
}
