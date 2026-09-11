/// Module responsible for notifying plugins of transactions received on the RPC
/// `sendTransaction` path.
use {
    crate::geyser_plugin_manager::GeyserPluginManager,
    agave_geyser_plugin_interface::geyser_plugin_interface::{
        ReceivedTransactionInfo, ReceivedTransactionInfoVersions,
    },
    arc_swap::ArcSwap,
    log::*,
    solana_clock::Slot,
    solana_rpc::received_transaction_notifier_interface::ReceivedTransactionNotifier,
    solana_signature::Signature,
    std::sync::Arc,
};

/// This implementation of ReceivedTransactionNotifier is passed to the rpc's request
/// processor at the validator startup. The request processor invokes
/// notify_transaction_received when a transaction is admitted to the send transaction
/// service. The implementation in turn invokes notify_transaction_received of each plugin
/// enabled with received transaction notification managed by the GeyserPluginManager.
pub(crate) struct ReceivedTransactionNotifierImpl {
    plugin_manager: Arc<ArcSwap<GeyserPluginManager>>,
}

impl ReceivedTransactionNotifier for ReceivedTransactionNotifierImpl {
    fn notify_transaction_received(
        &self,
        signature: &Signature,
        wire_transaction: &[u8],
        received_ns: u64,
        slot_hint: Slot,
        preflight_skipped: bool,
    ) {
        let plugin_manager = self.plugin_manager.load();

        if plugin_manager.plugins.is_empty() {
            return;
        }

        let transaction_info = ReceivedTransactionInfo {
            signature,
            transaction: wire_transaction,
            received_ns,
            slot_hint,
            preflight_skipped,
        };

        for plugin in plugin_manager.plugins.iter() {
            if !plugin.transaction_received_notifications_enabled() {
                continue;
            }
            match plugin.notify_transaction_received(ReceivedTransactionInfoVersions::V0_0_1(
                &transaction_info,
            )) {
                Err(err) => {
                    error!(
                        "Failed to notify received transaction, error: ({}) to plugin {}",
                        err,
                        plugin.name()
                    )
                }
                Ok(_) => {
                    trace!(
                        "Successfully notified received transaction to plugin {}",
                        plugin.name()
                    );
                }
            }
        }
    }
}

impl ReceivedTransactionNotifierImpl {
    pub fn new(plugin_manager: Arc<ArcSwap<GeyserPluginManager>>) -> Self {
        Self { plugin_manager }
    }
}
