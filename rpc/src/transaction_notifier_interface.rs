use {
    solana_clock::Slot, solana_signature::Signature,
    solana_transaction::sanitized::SanitizedTransaction,
    solana_transaction_status::TransactionStatusMeta, std::sync::Arc,
};

pub trait TransactionNotifier {
    fn notify_transaction(
        &self,
        slot: Slot,
        transaction_slot_index: usize,
        signature: &Signature,
        transaction_status_meta: &TransactionStatusMeta,
        transaction: &SanitizedTransaction,
    );
}

pub type TransactionNotifierArc = Arc<dyn TransactionNotifier + Sync + Send>;
