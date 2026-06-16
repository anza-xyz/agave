use {
    solana_clock::Slot, solana_hash::Hash, solana_ledger::blockstore::UpdateParentSignal,
    solana_signature::Signature, solana_transaction::versioned::VersionedTransaction,
    solana_transaction_status::TransactionStatusMeta, std::sync::Arc,
};

pub trait TransactionNotifier {
    fn notify_transaction(
        &self,
        slot: Slot,
        transaction_slot_index: usize,
        signature: &Signature,
        message_hash: &Hash,
        is_vote: bool,
        transaction_status_meta: &TransactionStatusMeta,
        transaction: &VersionedTransaction,
    );

    /// Called when an Alpenglow UpdateParent marker invalidates transaction
    /// status data for the same slot that may have been emitted before the
    /// marker boundary. Delivery is best-effort and ordered by the
    /// TransactionStatusService message channel.
    fn notify_update_parent(&self, update_parent: &UpdateParentSignal);
}

pub type TransactionNotifierArc = Arc<dyn TransactionNotifier + Sync + Send>;
