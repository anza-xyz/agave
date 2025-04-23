use {
    solana_pubkey::Pubkey,
    solana_sdk::{
        account::AccountSharedData, clock::Slot, signature::Signature,
        transaction::SanitizedTransaction,
    },
    solana_transaction_status::TransactionStatusMeta,
    std::sync::Arc,
};

pub trait TransactionNotifier {
    fn notify_transaction(
        &self,
        slot: Slot,
        transaction_slot_index: usize,
        signature: &Signature,
        transaction_status_meta: &TransactionStatusMeta,
        transaction: &SanitizedTransaction,
        post_accounts_states: &[(Pubkey, AccountSharedData)],
    );
}

pub type TransactionNotifierArc = Arc<dyn TransactionNotifier + Sync + Send>;
