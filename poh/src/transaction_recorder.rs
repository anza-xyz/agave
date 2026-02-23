use {
    crate::{
        poh_recorder::{PohRecorderError, Record},
        record_channels::{RecordSender, RecordSenderError},
    },
    solana_clock::BankId,
    solana_entry::entry::hash_transactions,
    solana_hash::Hash,
    solana_measure::measure_us,
    solana_transaction::versioned::VersionedTransaction,
    std::num::Saturating,
};

#[derive(Default, Debug)]
pub struct RecordTransactionsTimings {
    pub processing_results_to_transactions_us: Saturating<u64>,
    pub hash_us: Saturating<u64>,
    pub poh_record_us: Saturating<u64>,
}

impl RecordTransactionsTimings {
    pub fn accumulate(&mut self, other: &RecordTransactionsTimings) {
        self.processing_results_to_transactions_us += other.processing_results_to_transactions_us;
        self.hash_us += other.hash_us;
        self.poh_record_us += other.poh_record_us;
    }
}

pub struct RecordTransactionsSummary {
    // Metrics describing how time was spent recording transactions
    pub record_transactions_timings: RecordTransactionsTimings,
    // Result of trying to record the transactions into the PoH stream
    pub result: Result<(), PohRecorderError>,
    // Index in the slot of the first transaction recorded
    pub starting_transaction_index: Option<usize>,
}

/// Adds transactions into the PoH stream, by sending them to the PoH service.
#[derive(Clone, Debug)]
pub struct TransactionRecorder {
    // shared by all users of PohRecorder
    pub record_sender: RecordSender,
}

impl TransactionRecorder {
    pub fn new(record_sender: RecordSender) -> Self {
        Self { record_sender }
    }

    /// Hashes `transactions` and sends to PoH service for recording. Waits for response up to 1s.
    /// Panics on unexpected (non-`MaxHeightReached`) errors.
    pub fn record_transactions(
        &self,
        bank_id: BankId,
        transactions: Vec<VersionedTransaction>,
    ) -> RecordTransactionsSummary {
        let mut record_transactions_timings = RecordTransactionsTimings::default();
        let mut starting_transaction_index = None;

        if !transactions.is_empty() {
            let (hash, hash_us) = measure_us!(hash_transactions(&transactions));
            record_transactions_timings.hash_us = Saturating(hash_us);

            let (res, poh_record_us) =
                measure_us!(self.record(bank_id, vec![hash], vec![transactions]));
            record_transactions_timings.poh_record_us = Saturating(poh_record_us);

            match res {
                Ok(starting_index) => {
                    starting_transaction_index = starting_index;
                }
                Err(RecordSenderError::InactiveBankId | RecordSenderError::Shutdown) => {
                    return RecordTransactionsSummary {
                        record_transactions_timings,
                        result: Err(PohRecorderError::MaxHeightReached),
                        starting_transaction_index: None,
                    };
                }
                Err(RecordSenderError::Full) => {
                    return RecordTransactionsSummary {
                        record_transactions_timings,
                        result: Err(PohRecorderError::ChannelFull),
                        starting_transaction_index: None,
                    };
                }
                Err(RecordSenderError::Disconnected) => {
                    return RecordTransactionsSummary {
                        record_transactions_timings,
                        result: Err(PohRecorderError::ChannelDisconnected),
                        starting_transaction_index: None,
                    };
                }
            }
        }

        RecordTransactionsSummary {
            record_transactions_timings,
            result: Ok(()),
            starting_transaction_index,
        }
    }

    // Returns the index of `transactions.first()` in the slot, if being tracked by WorkingBank
    pub fn record(
        &self,
        bank_id: BankId,
        mixins: Vec<Hash>,
        transaction_batches: Vec<Vec<VersionedTransaction>>,
    ) -> Result<Option<usize>, RecordSenderError> {
        self.record_sender
            .try_send(Record::new(mixins, transaction_batches, bank_id))
    }
}
