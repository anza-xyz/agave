use {
    solana_clock::Slot, solana_message::v0::LoadedAddresses, solana_signature::Signature,
    solana_transaction::versioned::VersionedTransaction, std::sync::Arc,
};

/// Trait for notifying about transactions when they are deshredded.
/// This is called when entries are formed from shreds, before any execution occurs.
pub trait DeshredTransactionNotifier {
    fn notify_deshred_transaction(
        &self,
        slot: Slot,
        signature: &Signature,
        is_vote: bool,
        transaction: &VersionedTransaction,
        loaded_addresses: Option<&LoadedAddresses>,
    );
}

pub type DeshredTransactionNotifierArc = Arc<dyn DeshredTransactionNotifier + Sync + Send>;
