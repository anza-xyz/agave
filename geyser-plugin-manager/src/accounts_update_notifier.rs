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
    solana_clock::Slot,
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
        account: &AccountSharedData,
        txn: &Option<&SanitizedTransaction>,
        pubkey: &Pubkey,
        write_version: u64,
    ) {
        let account_info =
            self.accountinfo_from_shared_account_data(account, txn, pubkey, write_version);
        self.notify_plugins_of_account_update(account_info, slot, false);
    }

    fn notify_account_restore_from_snapshot(
        &self,
        slot: Slot,
        write_version: u64,
        account: &AccountForGeyser<'_>,
    ) {
        let mut account = self.accountinfo_from_account_for_geyser(account);
        account.write_version = write_version;
        self.notify_plugins_of_account_update(account, slot, true);
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

    fn notify_plugins_of_account_update(
        &self,
        account: ReplicaAccountInfoV3,
        slot: Slot,
        is_startup: bool,
    ) {
        let plugin_manager = self.plugin_manager.load();

        if plugin_manager.plugins.is_empty() {
            return;
        }
        for plugin in plugin_manager.plugins.iter() {
            if !plugin.account_data_notifications_enabled() {
                continue;
            }
            match plugin.update_account(
                ReplicaAccountInfoVersions::V0_0_3(&account),
                slot,
                is_startup,
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
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    #[derive(Debug)]
    struct TestAccountPlugin {
        name: &'static str,
        account_updates_enabled: bool,
        account_update_count: Arc<AtomicUsize>,
    }

    impl GeyserPlugin for TestAccountPlugin {
        fn name(&self) -> &'static str {
            self.name
        }

        fn update_account(
            &self,
            _account: ReplicaAccountInfoVersions,
            _slot: Slot,
            _is_startup: bool,
        ) -> agave_geyser_plugin_interface::geyser_plugin_interface::Result<()> {
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
        let plugin_manager = Arc::new(ArcSwap::from(Arc::new(GeyserPluginManager {
            plugins: vec![
                loaded_test_plugin(TestAccountPlugin {
                    name: "enabled",
                    account_updates_enabled: true,
                    account_update_count: enabled_count.clone(),
                }),
                loaded_test_plugin(TestAccountPlugin {
                    name: "disabled",
                    account_updates_enabled: false,
                    account_update_count: disabled_count.clone(),
                }),
            ],
        })));
        let notifier = AccountsUpdateNotifierImpl::new(plugin_manager, false);
        let account = AccountSharedData::new(1, 0, &Pubkey::new_unique());
        let pubkey = Pubkey::new_unique();

        notifier.notify_account_update(42, &account, &None, &pubkey, 7);

        assert_eq!(enabled_count.load(Ordering::Relaxed), 1);
        assert_eq!(disabled_count.load(Ordering::Relaxed), 0);
    }
}
