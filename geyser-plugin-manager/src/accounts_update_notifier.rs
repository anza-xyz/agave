/// Module responsible for notifying plugins of account updates
use {
    crate::geyser_plugin_manager::GeyserPluginManager,
    agave_geyser_plugin_interface::geyser_plugin_interface::{
        ReplicaAccountInfoV3, ReplicaAccountInfoVersions,
    },
    arc_swap::ArcSwap,
    log::*,
    solana_account::{AccountSharedData, ReadableAccount},
    solana_accounts_db::accounts_update_notifier_interface::{
        AccountForGeyser, AccountsUpdateNotifierInterface,
    },
    solana_clock::{BankId, Slot},
    solana_pubkey::Pubkey,
    solana_transaction::sanitized::SanitizedTransaction,
    std::sync::Arc,
};
#[derive(Debug)]
pub(crate) struct AccountsUpdateNotifierImpl {
    plugin_manager: Arc<ArcSwap<GeyserPluginManager>>,
    snapshot_notifications_enabled: bool,
}

impl AccountsUpdateNotifierInterface for AccountsUpdateNotifierImpl {
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
        if plugin_manager.plugins.is_empty() {
            return;
        }

        for plugin in plugin_manager.plugins.iter() {
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
        plugin_manager: Arc<ArcSwap<GeyserPluginManager>>,
        snapshot_notifications_enabled: bool,
    ) -> Self {
        AccountsUpdateNotifierImpl {
            plugin_manager,
            snapshot_notifications_enabled,
        }
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

        if plugin_manager.plugins.is_empty() {
            return;
        }
        for plugin in plugin_manager.plugins.iter() {
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

        if plugin_manager.plugins.is_empty() {
            return;
        }
        for plugin in plugin_manager.plugins.iter() {
            if !plugin.account_data_notifications_enabled() {
                continue;
            }
            match plugin.update_account_for_bank(
                ReplicaAccountInfoVersions::V0_0_3(&account),
                slot,
                bank_id,
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
        crate::geyser_plugin_manager::{GeyserPluginManager, LoadedGeyserPlugin},
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
        account_update_bank_ids: Arc<Mutex<Vec<BankId>>>,
    }

    impl GeyserPlugin for TestAccountPlugin {
        fn name(&self) -> &'static str {
            self.name
        }

        fn update_account_for_bank(
            &self,
            account: ReplicaAccountInfoVersions,
            _slot: Slot,
            bank_id: BankId,
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

    fn loaded_test_plugin(plugin: TestAccountPlugin) -> Arc<LoadedGeyserPlugin> {
        #[cfg(unix)]
        let library = libloading::os::unix::Library::this();
        #[cfg(windows)]
        let library = libloading::os::windows::Library::this().unwrap();

        Arc::new(LoadedGeyserPlugin::new(
            Library::from(library),
            Box::new(plugin),
            None,
        ))
    }

    #[test]
    fn test_notify_account_update_skips_plugins_with_account_notifications_disabled() {
        let enabled_count = Arc::new(AtomicUsize::new(0));
        let disabled_count = Arc::new(AtomicUsize::new(0));
        let enabled_bank_ids = Arc::new(Mutex::new(Vec::new()));
        let disabled_bank_ids = Arc::new(Mutex::new(Vec::new()));
        let plugin_manager = Arc::new(ArcSwap::from(Arc::new(GeyserPluginManager {
            plugins: vec![
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
        })));
        let notifier = AccountsUpdateNotifierImpl::new(plugin_manager, false);
        let account = AccountSharedData::new(1, 0, &Pubkey::new_unique());
        let pubkey = Pubkey::new_unique();
        let bank_id = 9;

        notifier.notify_account_update(42, bank_id, &account, &None, &pubkey, 7);

        assert_eq!(enabled_count.load(Ordering::Relaxed), 1);
        assert_eq!(disabled_count.load(Ordering::Relaxed), 0);
        assert_eq!(*enabled_bank_ids.lock().unwrap(), vec![bank_id]);
        assert!(disabled_bank_ids.lock().unwrap().is_empty());
    }

    #[test]
    fn test_notify_account_restore_from_snapshot_has_no_bank_id() {
        let account_update_count = Arc::new(AtomicUsize::new(0));
        let account_update_bank_ids = Arc::new(Mutex::new(Vec::new()));
        let plugin_manager = Arc::new(ArcSwap::from(Arc::new(GeyserPluginManager {
            plugins: vec![loaded_test_plugin(TestAccountPlugin {
                name: "enabled",
                account_updates_enabled: true,
                account_update_count: account_update_count.clone(),
                account_update_bank_ids: account_update_bank_ids.clone(),
            })],
        })));
        let notifier = AccountsUpdateNotifierImpl::new(plugin_manager, true);
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
        assert_eq!(
            *account_update_bank_ids.lock().unwrap(),
            Vec::<BankId>::new()
        );
    }
}
