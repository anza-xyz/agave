/// Module responsible for notifying plugins about entries
use {
    crate::geyser_plugin_manager::GeyserPluginManager,
    agave_geyser_plugin_interface::geyser_plugin_interface::{
        ReplicaEntryInfoV2, ReplicaEntryInfoVersions,
    },
    log::*,
    solana_clock::Slot,
    solana_entry::entry::EntrySummary,
    solana_ledger::entry_notifier_interface::EntryNotifier,
    std::sync::{Arc, RwLock},
};

pub(crate) struct EntryNotifierImpl {
    plugin_manager: Arc<RwLock<GeyserPluginManager>>,
}

impl EntryNotifier for EntryNotifierImpl {
    fn notify_entry<'a>(
        &'a self,
        slot: Slot,
        index: usize,
        entry: &'a EntrySummary,
        starting_transaction_index: usize,
    ) {
        let plugin_manager = self.plugin_manager.read().unwrap();
        if plugin_manager.plugins.is_empty() {
            return;
        }

        let entry_info =
            Self::build_replica_entry_info(slot, index, entry, starting_transaction_index);

        for plugin in plugin_manager.plugins.iter() {
            if !plugin.entry_notifications_enabled() {
                continue;
            }
            match plugin.notify_entry(ReplicaEntryInfoVersions::V0_0_2(&entry_info)) {
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
    pub fn new(plugin_manager: Arc<RwLock<GeyserPluginManager>>) -> Self {
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
