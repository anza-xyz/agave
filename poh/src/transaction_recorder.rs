use {
    crate::poh_recorder::{PohRecorderError, Record, Result},
    crossbeam_channel::{bounded, RecvTimeoutError, Sender},
    solana_clock::Slot,
    solana_entry::entry::hash_transactions,
    solana_hash::Hash,
    solana_measure::measure_us,
    solana_transaction::versioned::VersionedTransaction,
    std::{
        num::Saturating,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        time::Duration,
    },
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
    pub result: Result<()>,
    // Index in the slot of the first transaction recorded
    pub starting_transaction_index: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct TransactionRecorder {
    // shared by all users of PohRecorder
    pub record_sender: Sender<Record>,
    pub is_exited: Arc<AtomicBool>,
}

impl TransactionRecorder {
    pub fn new(record_sender: Sender<Record>, is_exited: Arc<AtomicBool>) -> Self {
        Self {
            record_sender,
            is_exited,
        }
    }

    /// Hashes `transactions` and sends to PoH service for recording. Waits for response up to 1s.
    /// Panics on unexpected (non-`MaxHeightReached`) errors.
    pub fn record_transactions(
        &self,
        bank_slot: Slot,
        transactions: Vec<VersionedTransaction>,
    ) -> RecordTransactionsSummary {
        let mut record_transactions_timings = RecordTransactionsTimings::default();
        let mut starting_transaction_index = None;

        if !transactions.is_empty() {
            let (hash, hash_us) = measure_us!(hash_transactions(&transactions));
            record_transactions_timings.hash_us = Saturating(hash_us);

            let (res, poh_record_us) = measure_us!(self.record(bank_slot, hash, transactions));
            record_transactions_timings.poh_record_us = Saturating(poh_record_us);

            match res {
                Ok(starting_index) => {
                    starting_transaction_index = starting_index;
                }
                Err(PohRecorderError::MaxHeightReached) => {
                    return RecordTransactionsSummary {
                        record_transactions_timings,
                        result: Err(PohRecorderError::MaxHeightReached),
                        starting_transaction_index: None,
                    };
                }
                Err(PohRecorderError::SendError(e)) => {
                    return RecordTransactionsSummary {
                        record_transactions_timings,
                        result: Err(PohRecorderError::SendError(e)),
                        starting_transaction_index: None,
                    };
                }
                Err(e) => panic!("Poh recorder returned unexpected error: {e:?}"),
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
        bank_slot: Slot,
        mixin: Hash,
        transactions: Vec<VersionedTransaction>,
    ) -> Result<Option<usize>> {
        // create a new channel so that there is only 1 sender and when it goes out of scope, the receiver fails
        let (result_sender, result_receiver) = bounded(1);
        let res =
            self.record_sender
                .send(Record::new(mixin, transactions, bank_slot, result_sender));
        if res.is_err() {
            // If the channel is dropped, then the validator is shutting down so return that we are hitting
            //  the max tick height to stop transaction processing and flush any transactions in the pipeline.
            return Err(PohRecorderError::MaxHeightReached);
        }
        // Besides validator exit, this timeout should primarily be seen to affect test execution environments where the various pieces can be shutdown abruptly
        let mut is_exited = false;
        loop {
            let res = result_receiver.recv_timeout(Duration::from_millis(1000));
            match res {
                Err(RecvTimeoutError::Timeout) => {
                    if is_exited {
                        return Err(PohRecorderError::MaxHeightReached);
                    } else {
                        // A result may have come in between when we timed out checking this
                        // bool, so check the channel again, even if is_exited == true
                        is_exited = self.is_exited.load(Ordering::SeqCst);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(PohRecorderError::MaxHeightReached);
                }
                Ok(result) => {
                    return result;
                }
            }
        }
    }
}
