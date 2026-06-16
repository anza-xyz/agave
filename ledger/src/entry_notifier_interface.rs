use {
    crate::blockstore::UpdateParentSignal, solana_clock::Slot, solana_entry::entry::EntrySummary,
    std::sync::Arc,
};

pub trait EntryNotifier {
    fn notify_entry(
        &self,
        slot: Slot,
        index: usize,
        entry: &EntrySummary,
        starting_transaction_index: usize,
    );

    /// Called when an Alpenglow UpdateParent marker invalidates same-slot
    /// entries and transactions observed before `update_parent_fec_set_index`.
    ///
    /// The callback is delivered on the same channel as entry notifications, so
    /// consumers observe it before later replacement entries sent by this
    /// service. Delivery remains best-effort under channel shutdown.
    fn notify_update_parent(&self, _update_parent: &UpdateParentSignal) {}
}

pub type EntryNotifierArc = Arc<dyn EntryNotifier + Sync + Send>;
