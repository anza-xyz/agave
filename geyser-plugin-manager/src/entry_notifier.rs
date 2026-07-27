/// Module responsible for notifying plugins about entries
use {
    crate::geyser_plugin_manager::GeyserPluginManager,
    agave_geyser_plugin_interface::geyser_plugin_interface::{
        ReplicaEntryInfoV2, ReplicaEntryInfoVersions,
    },
    arc_swap::ArcSwap,
    log::*,
    solana_clock::{BankId, Slot},
    solana_entry::entry::EntrySummary,
    solana_ledger::entry_notifier_interface::EntryNotifier,
    std::sync::Arc,
};

pub(crate) struct EntryNotifierImpl {
    plugin_manager: Arc<ArcSwap<GeyserPluginManager>>,
}

impl EntryNotifier for EntryNotifierImpl {
    fn notify_entry<'a>(
        &'a self,
        slot: Slot,
        bank_id: BankId,
        index: usize,
        entry: &'a EntrySummary,
        starting_transaction_index: usize,
    ) {
        let plugin_manager = self.plugin_manager.load();
        if plugin_manager.plugins.is_empty() {
            return;
        }

        let entry_info =
            Self::build_replica_entry_info(slot, index, entry, starting_transaction_index);

        for plugin in plugin_manager.plugins.iter() {
            if !plugin.entry_notifications_enabled() {
                continue;
            }
            match plugin
                .notify_entry_for_bank(ReplicaEntryInfoVersions::V0_0_2(&entry_info), bank_id)
            {
                Err(err) => {
                    error!(
                        "Failed to notify entry, error: ({}) to plugin {}",
                        err,
                        plugin.name()
                    )
                }
                Ok(_) => {
                    trace!("Successfully notified entry to plugin {}", plugin.name());
                }
            }
        }
    }
}

impl EntryNotifierImpl {
    pub fn new(plugin_manager: Arc<ArcSwap<GeyserPluginManager>>) -> Self {
        Self { plugin_manager }
    }

    fn build_replica_entry_info(
        slot: Slot,
        index: usize,
        entry: &'_ EntrySummary,
        starting_transaction_index: usize,
    ) -> ReplicaEntryInfoV2<'_> {
        ReplicaEntryInfoV2 {
            slot,
            index,
            num_hashes: entry.num_hashes,
            hash: entry.hash.as_ref(),
            executed_transaction_count: entry.num_transactions,
            starting_transaction_index,
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::geyser_plugin_manager::{GeyserPluginManager, LoadedGeyserPlugin},
        agave_geyser_plugin_interface::geyser_plugin_interface::{GeyserPlugin, Result},
        arc_swap::ArcSwap,
        libloading::Library,
        solana_hash::Hash,
        std::sync::{Arc, Mutex},
    };

    type EntryUpdate = (Slot, BankId, usize, usize);

    #[derive(Debug)]
    struct TestEntryPlugin {
        entry_notifications_enabled: bool,
        updates: Arc<Mutex<Vec<EntryUpdate>>>,
    }

    impl GeyserPlugin for TestEntryPlugin {
        fn name(&self) -> &'static str {
            "test-entry-plugin"
        }

        fn notify_entry_for_bank(
            &self,
            entry: ReplicaEntryInfoVersions,
            bank_id: BankId,
        ) -> Result<()> {
            let ReplicaEntryInfoVersions::V0_0_2(entry) = entry else {
                panic!("expected V0_0_2 entry info");
            };
            self.updates.lock().unwrap().push((
                entry.slot,
                bank_id,
                entry.index,
                entry.starting_transaction_index,
            ));
            Ok(())
        }

        fn entry_notifications_enabled(&self) -> bool {
            self.entry_notifications_enabled
        }
    }

    fn loaded_test_plugin(plugin: TestEntryPlugin) -> Arc<LoadedGeyserPlugin> {
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
    fn test_notify_entry_includes_bank_id() {
        let enabled_updates = Arc::new(Mutex::new(Vec::new()));
        let disabled_updates = Arc::new(Mutex::new(Vec::new()));
        let plugin_manager = Arc::new(ArcSwap::from(Arc::new(GeyserPluginManager {
            plugins: vec![
                loaded_test_plugin(TestEntryPlugin {
                    entry_notifications_enabled: true,
                    updates: enabled_updates.clone(),
                }),
                loaded_test_plugin(TestEntryPlugin {
                    entry_notifications_enabled: false,
                    updates: disabled_updates.clone(),
                }),
            ],
        })));
        let notifier = EntryNotifierImpl::new(plugin_manager);
        let entry = EntrySummary {
            num_hashes: 1,
            hash: Hash::new_unique(),
            num_transactions: 2,
        };

        notifier.notify_entry(42, 9, 3, &entry, 7);

        assert_eq!(*enabled_updates.lock().unwrap(), vec![(42, 9, 3, 7)]);
        assert!(disabled_updates.lock().unwrap().is_empty());
    }
}
