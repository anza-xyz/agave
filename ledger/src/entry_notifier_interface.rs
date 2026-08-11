use {
    agave_votor_messages::consensus_message::BlockId,
    solana_clock::{BankId, Slot},
    solana_entry::{block_component::VersionedBlockFooter, entry::EntrySummary},
    std::sync::Arc,
};

#[derive(Debug, Eq, PartialEq)]
pub struct EntryUpdateParentInfo {
    pub slot: Slot,
    pub cleared_bank_id: BankId,
    pub parent_slot: Slot,
    pub parent_block_id: BlockId,
}

pub trait EntryNotifier {
    fn notify_entry(
        &self,
        slot: Slot,
        bank_id: BankId,
        index: usize,
        entry: &EntrySummary,
        starting_transaction_index: usize,
    );

    /// Notify an Alpenglow UpdateParent marker on the entry stream.
    fn notify_entry_update_parent(&self, _update_parent: &EntryUpdateParentInfo) {}

    /// Notify an Alpenglow block footer.
    ///
    /// This callback is delivered on the same channel as entry notifications,
    /// preserving the order of entries and the footer within the block.
    fn notify_block_footer(
        &self,
        _slot: Slot,
        _bank_id: BankId,
        _block_footer: &VersionedBlockFooter,
    ) {
    }
}

pub type EntryNotifierArc = Arc<dyn EntryNotifier + Sync + Send>;
