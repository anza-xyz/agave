use {
    crate::blockstore::UpdateParentSignal, solana_clock::Slot, solana_message::v0::LoadedAddresses,
    solana_signature::Signature, solana_transaction::versioned::VersionedTransaction,
    std::sync::Arc,
};

/// Trait for notifying about transactions when they are deshredded.
/// This is called when entries are formed from shreds, before any execution occurs.
///
/// The completed-data-set shred range identifies the contiguous range of data shreds whose
/// combined payload deserializes to a single `Vec<Entry>`. All transactions reconstructed from
/// that same completed data set share the same shred-range metadata.
pub trait DeshredTransactionNotifier {
    fn notify_deshred_transaction(
        &self,
        slot: Slot,
        completed_data_set_starting_shred_index: u32,
        completed_data_set_ending_shred_index_exclusive: u32,
        signature: &Signature,
        is_vote: bool,
        transaction: &VersionedTransaction,
        loaded_addresses: Option<&LoadedAddresses>,
    );

    /// Whether any plugin has opted in to ALT resolution for deshred transactions.
    fn alt_resolution_enabled(&self) -> bool;

    /// Called when an Alpenglow UpdateParent marker invalidates same-slot
    /// deshred transactions emitted before `update_parent_fec_set_index`.
    ///
    /// Delivery is best-effort and ordered with completed-data-set processing
    /// by CompletedDataSetsService.
    fn notify_update_parent(&self, _update_parent: &UpdateParentSignal) {}
}

pub type DeshredTransactionNotifierArc = Arc<dyn DeshredTransactionNotifier + Sync + Send>;
