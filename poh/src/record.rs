use {
    crate::poh_record_error::Result, crossbeam_channel::Sender, solana_clock::Slot,
    solana_hash::Hash, solana_transaction::versioned::VersionedTransaction,
};

// Sends the Result of the record operation, including the index in the slot of the first
// transaction, if being tracked by WorkingBank
type RecordResultSender = Sender<Result<Option<usize>>>;

pub struct Record {
    pub mixin: Hash,
    pub transactions: Vec<VersionedTransaction>,
    pub slot: Slot,
    pub sender: RecordResultSender,
}
impl Record {
    pub fn new(
        mixin: Hash,
        transactions: Vec<VersionedTransaction>,
        slot: Slot,
        sender: RecordResultSender,
    ) -> Self {
        Self {
            mixin,
            transactions,
            slot,
            sender,
        }
    }
}
