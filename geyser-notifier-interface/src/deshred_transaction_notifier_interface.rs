use {
    solana_clock::Slot, solana_hash::Hash, solana_message::v0::LoadedAddresses,
    solana_signature::Signature, solana_transaction::versioned::VersionedTransaction,
    std::sync::Arc,
};

/// Describes an UpdateParent marker observed for a slot: replay of the slot
/// must skip the optimistic-parent prefix before `update_parent_fec_set_index`
/// and use (`parent_slot`, `parent_block_id`) as the effective parent.
#[derive(Debug, Eq, PartialEq)]
pub struct UpdateParentInfo {
    pub slot: Slot,
    pub update_parent_fec_set_index: u32,
    pub parent_slot: Slot,
    pub parent_block_id: Hash,
}

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

    /// Notify that an UpdateParent marker replaced earlier same-slot data.
    fn notify_deshred_update_parent(&self, _update_parent: &UpdateParentInfo) {}
}

pub type DeshredTransactionNotifierArc = Arc<dyn DeshredTransactionNotifier + Sync + Send>;
