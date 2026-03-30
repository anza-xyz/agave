/// Module responsible for notifying plugins of transactions when deshredded
use {
    crate::geyser_plugin_manager::GeyserPluginManager,
    agave_geyser_plugin_interface::geyser_plugin_interface::{
        ReplicaDeshredTransactionInfo, ReplicaDeshredTransactionInfoVersions,
    },
    arc_swap::ArcSwap,
    log::*,
    solana_clock::Slot,
    solana_ledger::deshred_transaction_notifier_interface::DeshredTransactionNotifier,
    solana_measure::measure::Measure,
    solana_message::v0::LoadedAddresses,
    solana_metrics::*,
    solana_signature::Signature,
    solana_transaction::versioned::VersionedTransaction,
    std::sync::Arc,
};

/// This implementation of DeshredTransactionNotifier is passed to the CompletedDataSetsService
/// at validator startup. CompletedDataSetsService invokes the notify_deshred_transaction method
/// when entries are formed from shreds. The implementation in turn invokes the
/// notify_deshred_transaction of each plugin enabled with deshred transaction notification
/// managed by the GeyserPluginManager.
pub(crate) struct DeshredTransactionNotifierImpl {
    plugin_manager: Arc<ArcSwap<GeyserPluginManager>>,
}

impl DeshredTransactionNotifier for DeshredTransactionNotifierImpl {
    fn notify_deshred_transaction(
        &self,
        slot: Slot,
        signature: &Signature,
        is_vote: bool,
        transaction: &VersionedTransaction,
        loaded_addresses: Option<&LoadedAddresses>,
    ) {
        let plugin_manager = self.plugin_manager.load();

        if plugin_manager.plugins.is_empty() {
            return;
        }

        let mut measure =
            Measure::start("geyser-plugin-notify_plugins_of_deshred_transaction_info");
        let transaction_info = ReplicaDeshredTransactionInfo {
            signature,
            is_vote,
            transaction,
            loaded_addresses,
        };

        for plugin in plugin_manager.plugins.iter() {
            if !plugin.deshred_transaction_notifications_enabled() {
                continue;
            }
            match plugin.notify_deshred_transaction(
                ReplicaDeshredTransactionInfoVersions::V0_0_1(&transaction_info),
                slot,
            ) {
                Err(err) => {
                    error!(
                        "Failed to notify deshred transaction, error: ({}) to plugin {}",
                        err,
                        plugin.name()
                    )
                }
                Ok(_) => {
                    trace!(
                        "Successfully notified deshred transaction to plugin {}",
                        plugin.name()
                    );
                }
            }
        }
        measure.stop();
        inc_new_counter_debug!(
            "geyser-plugin-notify_plugins_of_deshred_transaction_info-us",
            measure.as_us() as usize,
            10000,
            10000
        );
    }

    fn alt_resolution_enabled(&self) -> bool {
        self.plugin_manager
            .load()
            .deshred_transaction_alt_resolution_enabled()
    }
}

impl DeshredTransactionNotifierImpl {
    pub fn new(plugin_manager: Arc<ArcSwap<GeyserPluginManager>>) -> Self {
        Self { plugin_manager }
    }
}
