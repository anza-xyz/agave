use {
    solana_clock::Slot, solana_hash::Hash, solana_signature::Signature,
    solana_transaction::versioned::VersionedTransaction,
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
}

pub type TransactionNotifierArc = Arc<dyn TransactionNotifier + Sync + Send>;
