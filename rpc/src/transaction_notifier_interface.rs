use {
    solana_account::AccountSharedData, solana_clock::Slot, solana_hash::Hash,
    solana_pubkey::Pubkey, solana_signature::Signature,
    solana_transaction::versioned::VersionedTransaction,
    solana_transaction_status::TransactionStatusMeta, std::sync::Arc,
};

pub trait TransactionNotifier {
    #[allow(clippy::too_many_arguments)]
    fn notify_transaction(
        &self,
        slot: Slot,
        transaction_slot_index: usize,
        signature: &Signature,
        message_hash: &Hash,
        is_vote: bool,
        transaction_status_meta: &TransactionStatusMeta,
        transaction: &VersionedTransaction,
        pre_accounts_states: &[(Pubkey, AccountSharedData)],
        post_accounts_states: &[(Pubkey, AccountSharedData)],
    );
}

pub type TransactionNotifierArc = Arc<dyn TransactionNotifier + Sync + Send>;
