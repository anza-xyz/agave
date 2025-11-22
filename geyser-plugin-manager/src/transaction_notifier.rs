/// Module responsible for notifying plugins of transactions
use {
    crate::geyser_plugin_manager::GeyserPluginManager,
    agave_geyser_plugin_interface::geyser_plugin_interface::{
        ReplicaTransactionInfoV3, ReplicaTransactionInfoVersions,
    },
    log::*,
    solana_clock::Slot,
    solana_hash::Hash,
    solana_rpc::transaction_notifier_interface::TransactionNotifier,
    solana_signature::Signature,
    solana_transaction::versioned::VersionedTransaction,
    solana_transaction_status::TransactionStatusMeta,
    std::sync::{Arc, RwLock},
};

/// This implementation of TransactionNotifier is passed to the rpc's TransactionStatusService
/// at the validator startup. TransactionStatusService invokes the notify_transaction method
/// for new transactions. The implementation in turn invokes the notify_transaction of each
/// plugin enabled with transaction notification managed by the GeyserPluginManager.
pub(crate) struct TransactionNotifierImpl {
    plugin_manager: Arc<RwLock<GeyserPluginManager>>,
}

impl TransactionNotifier for TransactionNotifierImpl {
    fn notify_transaction(
        &self,
        slot: Slot,
        index: usize,
        signature: &Signature,
        message_hash: &Hash,
        is_vote: bool,
        transaction_status_meta: &TransactionStatusMeta,
        transaction: &VersionedTransaction,
    ) {
        let transaction_log_info = Self::build_replica_transaction_info(
            index,
            signature,
            message_hash,
            is_vote,
            transaction_status_meta,
            transaction,
        );

        let plugin_manager = self.plugin_manager.read().unwrap();

        if plugin_manager.plugins.is_empty() {
            return;
        }

        for plugin in plugin_manager.plugins.iter() {
            if !plugin.transaction_notifications_enabled() {
                continue;
            }
            match plugin.notify_transaction(
                ReplicaTransactionInfoVersions::V0_0_3(&transaction_log_info),
                slot,
            ) {
                Err(err) => {
                    error!(
                        "Failed to notify transaction, error: ({}) to plugin {}",
                        err,
                        plugin.name()
                    )
                }
                Ok(_) => {
                    trace!(
                        "Successfully notified transaction to plugin {}",
                        plugin.name()
                    );
                }
            }
        }
    }
}

impl TransactionNotifierImpl {
    pub fn new(plugin_manager: Arc<RwLock<GeyserPluginManager>>) -> Self {
        Self { plugin_manager }
    }

    fn build_replica_transaction_info<'a>(
        index: usize,
        signature: &'a Signature,
        message_hash: &'a Hash,
        is_vote: bool,
        transaction_status_meta: &'a TransactionStatusMeta,
        transaction: &'a VersionedTransaction,
    ) -> ReplicaTransactionInfoV3<'a> {
        ReplicaTransactionInfoV3 {
            index,
            message_hash,
            signature,
            is_vote,
            transaction,
            transaction_status_meta,
        }
    }
}
