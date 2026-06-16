/// Module responsible for notifying plugins about entries
use {
    crate::geyser_plugin_manager::GeyserPluginManager,
    agave_geyser_plugin_interface::geyser_plugin_interface::{
        ReplicaEntryInfoV2, ReplicaEntryInfoVersions, ReplicaUpdateParentInfo,
        ReplicaUpdateParentInfoVersions,
    },
    arc_swap::ArcSwap,
    log::*,
    solana_clock::Slot,
    solana_entry::entry::EntrySummary,
    solana_ledger::{blockstore::UpdateParentSignal, entry_notifier_interface::EntryNotifier},
    std::sync::Arc,
};

pub(crate) struct EntryNotifierImpl {
    plugin_manager: Arc<ArcSwap<GeyserPluginManager>>,
}

impl EntryNotifier for EntryNotifierImpl {
    fn notify_entry<'a>(
        &'a self,
        slot: Slot,
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

    fn notify_update_parent(&self, update_parent: &UpdateParentSignal) {
        let plugin_manager = self.plugin_manager.load();
        if plugin_manager.plugins.is_empty() {
            return;
        }

        let update_parent_info = Self::build_replica_update_parent_info(update_parent);
        for plugin in plugin_manager.plugins.iter() {
            if !plugin.entry_notifications_enabled() {
                continue;
            }
            match plugin
                .notify_update_parent(ReplicaUpdateParentInfoVersions::V0_0_1(&update_parent_info))
            {
                Err(err) => error!(
                    "Failed to notify entry UpdateParent, error: ({}) to plugin {}",
                    err,
                    plugin.name()
                ),
                Ok(_) => trace!(
                    "Successfully notified entry UpdateParent to plugin {}",
                    plugin.name()
                ),
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

    fn build_replica_update_parent_info(
        update_parent: &UpdateParentSignal,
    ) -> ReplicaUpdateParentInfo<'_> {
        ReplicaUpdateParentInfo {
            slot: update_parent.slot,
            update_parent_fec_set_index: update_parent.update_parent_fec_set_index,
            parent_slot: update_parent.parent_slot,
            parent_block_id: update_parent.parent_block_id.as_ref(),
        }
    }
}
