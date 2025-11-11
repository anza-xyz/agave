#[cfg(feature = "shuttle-test")]
use shuttle::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
#[cfg(not(feature = "shuttle-test"))]
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use {
    crate::poh_recorder::Record,
    crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TryRecvError},
    solana_clock::BankId,
    std::time::Duration,
};

/// Create a channel pair for communicating [`Record`]s.
/// Transaction processing threads (workers/vote thread) send records, and
/// PohService receives them.
///
/// The receiver can shutdown the channel, preventing any further sends,
/// and can restart the channel for a new bank id, re-enabling sends.
/// The sender does not wait for the receiver to pick up records, and will return
/// immediately if the channel is full, shutdown, or if the bank id has changed.
///
/// The channel has a bounded capacity based on the maximum number of allowed
/// insertions at a given time. This is for guaranteeing that once shutdown the
/// service can always process all sent records correctly without dropping any
/// i.e. once sent records can be guaranteed to be recorded.
pub fn record_channels(track_transaction_indexes: bool) -> (RecordSender, RecordReceiver) {
    const CAPACITY: usize = RecordChannelStatus::MAX_ALLOWED_INSERTIONS as usize;
    let (sender, receiver) = bounded(CAPACITY);

    // Begin in a shutdown state.
    let status = RecordChannelStatus::new_shutdown(CAPACITY as u64);
    let transaction_indexes = if track_transaction_indexes {
        Some(Arc::new(Mutex::new(0)))
    } else {
        None
    };

    (
        RecordSender {
            status: status.clone(),
            sender,
            transaction_indexes: transaction_indexes.clone(),
        },
        RecordReceiver {
            status,
            receiver,
            capacity: CAPACITY as u64,
            transaction_indexes,
        },
    )
}

pub enum RecordSenderError {
    /// The channel is full, the record was not sent.
    Full,
    /// The channel is in a shutdown state, it is not valid to
    /// send records for this bank anymore.
    Shutdown,
    /// The record's bank id does not match the current bank id of the channel.
    InactiveBankId,
    /// The receiver has been dropped, the channel is disconnected.
    Disconnected,
}

/// A sender for sending [`Record`]s to PohService.
/// The sender does not wait for service to pick up the records, and will return
/// immediately if the channel is full, shutdown, or if the bank id has changed.
#[derive(Clone, Debug)]
pub struct RecordSender {
    /// Used to track active senders for the current bank id. Used so that the receiver
    /// side can determine that no more sends are in-flight while shutting down.
    status: RecordChannelStatus,
    sender: Sender<Record>,
    transaction_indexes: Option<Arc<Mutex<usize>>>,
}

impl RecordSender {
    pub fn try_send(&self, record: Record) -> Result<Option<usize>, RecordSenderError> {
        let num_transactions: usize = record
            .transaction_batches
            .iter()
            .map(|batch| batch.len())
            .sum();
        assert!(num_transactions > 0);
        loop {
            // Grab lock on `transaction_indexes` here to ensure we are sending
            // sequentially, ONLY if this exists.
            let transaction_indexes = self
                .transaction_indexes
                .as_ref()
                .map(|transaction_indexes| transaction_indexes.lock().unwrap());

            // Get the current bank_id and allowed insertions.
            // If the number of allowed insertions is less than the number of
            // batches, the channel is full - just return immediately.
            // If the `record`'s bank_id is different from the current bank_id,
            // return immediately.
            let current_status = self.status.0.load(Ordering::Acquire);
            let (is_shutdown, bank_id, allowed_insertions) = (
                RecordChannelStatus::is_shutdown(current_status),
                RecordChannelStatus::bank_id(current_status),
                RecordChannelStatus::allowed_insertions(current_status),
            );

            if is_shutdown {
                return Err(RecordSenderError::Shutdown);
            }
            if bank_id != record.bank_id {
                return Err(RecordSenderError::InactiveBankId);
            }
            if allowed_insertions < record.transaction_batches.len() as u64 {
                return Err(RecordSenderError::Full);
            }

            let new_status = RecordChannelStatus::encoded_value(
                false,
                bank_id,
                allowed_insertions.wrapping_sub(record.transaction_batches.len() as u64),
            );

            // PERF: Can just save the return value in a local to avoid needing to reload the value
            // on loop.
            if self
                .status
                .0
                .compare_exchange(
                    current_status,
                    new_status,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                // Failed to reserve space, re-acquire the current_bank_id_allowed_insertions &
                // retry.
                continue;
            }

            match self.sender.try_send(record) {
                Ok(_) => {
                    return Ok(transaction_indexes.map(|mut transaction_indexes| {
                        let transaction_starting_index = *transaction_indexes;
                        *transaction_indexes += num_transactions;
                        transaction_starting_index
                    }));
                }
                Err(err) => {
                    assert!(err.is_disconnected());
                    return Err(RecordSenderError::Disconnected);
                }
            }
        }
    }
}

/// A receiver for receiving [`Record`]s in PohService.
/// The receiver can shutdown the channel, preventing any further sends,
/// and can restart the channel for a new bank id, re-enabling sends.
pub struct RecordReceiver {
    capacity: u64,
    status: RecordChannelStatus,
    receiver: Receiver<Record>,
    transaction_indexes: Option<Arc<Mutex<usize>>>,
}

impl RecordReceiver {
    /// Returns true if the channel should be shutdown.
    pub fn should_shutdown(&self, remaining_hashes_in_slot: u64, ticks_per_slot: u64) -> bool {
        // This channel must guarantee that all sent records are recorded.
        // Each batch in a record consumes one hash in the PoH stream,
        // each tick also consumes at least one hash in the PoH stream.
        // As a conservative estimate, we assume no ticks have been recorded.
        remaining_hashes_in_slot.saturating_sub(ticks_per_slot) <= self.capacity
    }

    /// Shutdown the channel immediately.
    pub fn shutdown(&mut self) {
        self.status.shutdown();
    }

    /// Check if the channel is shutdown.
    pub fn is_shutdown(&self) -> bool {
        RecordChannelStatus::is_shutdown(self.status.0.load(Ordering::Acquire))
    }

    /// Re-enable the channel after a shutdown.
    pub fn restart(&mut self, bank_id: BankId) {
        assert!(bank_id <= RecordChannelStatus::MAX_BANK_ID);
        assert!(self.receiver.is_empty()); // Should be empty before restarting.
        assert!(self.is_shutdown());
        assert_eq!(self.status.load_allowed_insertions(), self.capacity);

        // Reset transaction indexes if tracking them - BEFORE allowing new insertions.
        let transaction_indexes_lock =
            self.transaction_indexes
                .as_ref()
                .map(|transaction_indexes| {
                    let mut lock = transaction_indexes.lock().unwrap();
                    *lock = 0;
                    lock
                });

        self.status.0.store(
            RecordChannelStatus::encoded_value(false, bank_id, self.capacity),
            Ordering::Release,
        );
        assert!(!self.is_shutdown());

        // Drop lock AFTER allowing new insertions. This makes any sends grabbing locks
        // wait until after the bank id has been changed. Meaning the CAS in try_send
        // will always succeed, if passing previous checks.
        drop(transaction_indexes_lock);
    }

    /// Drain all available records from the channel with `try_recv` loop.
    pub fn drain(&self) -> impl Iterator<Item = Record> + '_ {
        core::iter::from_fn(|| self.try_recv().ok())
    }

    /// Channel is empty and there are no active threads attempting to send.
    pub fn is_safe_to_restart(&self) -> bool {
        self.status.load_allowed_insertions() == self.capacity
    }

    /// Try to receive a record from the channel.
    pub fn try_recv(&self) -> Result<Record, TryRecvError> {
        // In order to avoid returning None when there was an active sender
        // we load `active_senders` prior to try_recv.
        let mut sender_active = self.status.load_allowed_insertions() < self.capacity;

        loop {
            match self.receiver.try_recv() {
                Ok(record) => {
                    self.on_received_record(record.transaction_batches.len() as u64);
                    return Ok(record);
                }
                Err(TryRecvError::Empty) => {
                    if sender_active {
                        // If the sender is STILL active then we must continue to wait.
                        // If there is no longer an active sender then we can break,
                        //   **after** checking the channel again.
                        // Both cases here are handled if we update `sender_active` and
                        // go to the next iteration of the loop.
                        sender_active = self.status.load_allowed_insertions() < self.capacity;
                        continue;
                    }
                    return Err(TryRecvError::Empty);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Receive a record from the channel, waiting up to `duration`.
    pub fn recv_timeout(&self, duration: Duration) -> Result<Record, RecvTimeoutError> {
        let record = self.receiver.recv_timeout(duration)?;
        self.on_received_record(record.transaction_batches.len() as u64);
        Ok(record)
    }

    fn on_received_record(&self, num_batches: u64) {
        // The record has been received and processed, so increment the number
        // of allowed insertions, so that new records can be sent.
        self.status.0.fetch_add(num_batches, Ordering::AcqRel);
    }
}

/// Encoded u64 where the upper 54 bits are the bank_id and the lower 10 bits are
/// the number of allowed insertions at the current time.
/// The number of allowed insertions is based on the number of **batches** sent,
/// not the number of [`Record`]. This is because each batch is a separate hash
/// in the PoH stream, and we must guarantee enough space for each hash, if we
/// allow a [`Record`] to be sent.
/// The allowed insertions uses 10 bits allowing up to 1023 insertions at a
/// given time. This is for messages that have been sent but not yet processed
/// by the receiver.
/// The `allowed_insertions` is a budget and is decremented when something is
/// sent/inserted into the channel, and incremented when something is received
/// from the channel.
#[derive(Clone, Debug)]
struct RecordChannelStatus(Arc<AtomicU64>);

impl RecordChannelStatus {
    /// Total bits.
    const NUM_BITS: u64 = 64;
    /// Bits used to track shutdown status.
    const SHUTDOWN_BITS: u64 = 1;
    /// Bits used to track allowed insertions.
    const ALLOWED_INSERTIONS_BITS: u64 = 10;
    /// Bits used to track bank ID.
    const BANK_ID_BITS: u64 = Self::NUM_BITS - Self::SHUTDOWN_BITS - Self::ALLOWED_INSERTIONS_BITS;

    const MAX_BANK_ID: BankId = (1 << Self::BANK_ID_BITS) - 1;
    const MAX_ALLOWED_INSERTIONS: u64 = (1 << Self::ALLOWED_INSERTIONS_BITS) - 1;

    const SHUTDOWN_MASK: u64 = 1 << 63;

    /// Create a new `BankIdAllowedInsertions` with state consistent with a
    /// shutdown state:
    /// - bank_id = `DISABLED_BANK_ID`
    /// - allowed_insertions = 0
    fn new_shutdown(capacity: u64) -> Self {
        Self(Arc::new(AtomicU64::new(Self::encoded_value(
            true, 0, capacity,
        ))))
    }

    /// Shutdown the channel immediately.
    fn shutdown(&self) {
        self.0.fetch_or(Self::SHUTDOWN_MASK, Ordering::Release);
    }

    fn load_allowed_insertions(&self) -> u64 {
        Self::allowed_insertions(self.0.load(Ordering::Acquire))
    }

    const fn encoded_value(shutdown: bool, bank_id: BankId, allowed_insertions: u64) -> u64 {
        assert!(bank_id <= Self::MAX_BANK_ID);
        assert!(allowed_insertions <= Self::MAX_ALLOWED_INSERTIONS);

        let shutdown = match shutdown {
            true => Self::SHUTDOWN_MASK,
            false => 0,
        };
        let bank_id = bank_id << Self::ALLOWED_INSERTIONS_BITS;

        shutdown | bank_id | allowed_insertions
    }

    /// Whether the current channel is in shutdown mode.
    fn is_shutdown(value: u64) -> bool {
        value & Self::SHUTDOWN_MASK != 0
    }

    /// The current bank_id, or [`Self::DISABLED_BANK_ID`] if shutdown.
    fn bank_id(value: u64) -> BankId {
        (value & !Self::SHUTDOWN_MASK) >> Self::ALLOWED_INSERTIONS_BITS
    }

    /// How many insertions/sends are allowed at this time.
    fn allowed_insertions(value: u64) -> u64 {
        value & Self::MAX_ALLOWED_INSERTIONS
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_hash::Hash, solana_transaction::versioned::VersionedTransaction};

    pub(super) fn test_record(bank_id: BankId, num_batches: usize) -> Record {
        Record {
            bank_id,
            transaction_batches: (0..num_batches)
                .map(|_| vec![VersionedTransaction::default()])
                .collect(),
            mixins: (0..num_batches).map(|_| Hash::default()).collect(),
        }
    }

    #[test]
    fn test_encoded_value() {
        let active = RecordChannelStatus::encoded_value(false, 100, 64);
        let bank_id = RecordChannelStatus::bank_id(active);
        let allowed_insertions = RecordChannelStatus::allowed_insertions(active);
        assert_eq!(bank_id, 100);
        assert_eq!(allowed_insertions, 64);
    }

    #[test]
    fn test_shutdown_lifecycle() {
        let status = RecordChannelStatus::new_shutdown(64);

        // Assert - Initializes shutdown.
        let val = status.0.load(Ordering::Relaxed);
        assert!(RecordChannelStatus::is_shutdown(val));

        // Assert - Storing a new encoded value clears shutdown.
        status.0.store(
            RecordChannelStatus::encoded_value(false, 100, 64),
            Ordering::Relaxed,
        );
        let val = status.0.load(Ordering::Relaxed);
        assert!(!RecordChannelStatus::is_shutdown(val));
        assert_eq!(RecordChannelStatus::bank_id(val), 100);
        assert_eq!(RecordChannelStatus::allowed_insertions(val), 64);

        // Assert - Shutdown flag doesn't clobber bank & insertions.
        status.shutdown();
        let val = status.0.load(Ordering::Relaxed);
        assert!(RecordChannelStatus::is_shutdown(val));
        assert_eq!(RecordChannelStatus::bank_id(val), 100);
        assert_eq!(RecordChannelStatus::allowed_insertions(val), 64);

        // Assert - Can set the next bank and reset the shutdown.
        status.0.store(
            RecordChannelStatus::encoded_value(false, 101, 64),
            Ordering::Relaxed,
        );
        let val = status.0.load(Ordering::Relaxed);
        assert!(!RecordChannelStatus::is_shutdown(val));
        assert_eq!(RecordChannelStatus::bank_id(val), 101);
        assert_eq!(RecordChannelStatus::allowed_insertions(val), 64);
    }

    #[test]
    fn test_record_channels() {
        let (sender, mut receiver) = record_channels(false);

        // Initially shutdown.
        assert!(matches!(
            sender.try_send(test_record(0, 1)),
            Err(RecordSenderError::Shutdown)
        ));

        // Restart for bank_id 1.
        receiver.restart(1);

        // Record for bank_id 0 fails.
        assert!(matches!(
            sender.try_send(test_record(0, 1)),
            Err(RecordSenderError::InactiveBankId)
        ));

        // Record for bank_id 1 with 1 batch succeeds.
        assert!(matches!(sender.try_send(test_record(1, 1)), Ok(None)));

        // Record for bank_id 1 with 1023 batches fails (channel full).
        assert!(matches!(
            sender.try_send(test_record(1, 1023)),
            Err(RecordSenderError::Full)
        ));

        // Record for bank_id 1 with 1022 batches succeeds (channel now full).
        assert!(matches!(sender.try_send(test_record(1, 1022)), Ok(None)));

        // Record for bank_id 1 with 1 batch fails (channel full).
        assert!(matches!(
            sender.try_send(test_record(1, 1)),
            Err(RecordSenderError::Full)
        ));

        // Receive 1 record.
        assert!(receiver.try_recv().is_ok());
        assert!(!receiver.is_safe_to_restart());
        assert!(receiver.try_recv().is_ok());
        assert!(receiver.is_safe_to_restart());
    }

    #[test]
    fn test_record_channels_track_indexes() {
        let (sender, mut receiver) = record_channels(true);

        // Initially shutdown.
        assert!(matches!(
            sender.try_send(test_record(0, 1)),
            Err(RecordSenderError::Shutdown)
        ));

        // Restart for bank_id 1.
        receiver.restart(1);

        // Record for bank_id 0 fails.
        assert!(matches!(
            sender.try_send(test_record(0, 1)),
            Err(RecordSenderError::InactiveBankId)
        ));

        // Record for bank_id 1 with 1 batch succeeds.
        assert!(matches!(sender.try_send(test_record(1, 1)), Ok(Some(0))));

        // Record for bank_id 1 with 2 batches (3 transactions) succeeds.
        let mut record = test_record(1, 2);
        record
            .transaction_batches
            .last_mut()
            .unwrap()
            .push(VersionedTransaction::default());
        assert!(matches!(sender.try_send(record), Ok(Some(1))));

        assert!(*sender.transaction_indexes.as_ref().unwrap().lock().unwrap() == 4);
    }
}

#[cfg(all(test, feature = "shuttle-test"))]
mod shuttle_tests {
    use super::{tests::test_record, *};

    #[test]
    fn test_sender_shutdown_safety_race() {
        const NUM_TEST_RUNS: usize = 100;
        shuttle::check_random(
            || {
                let (sender, mut receiver) = record_channels(false);

                const ITERATIONS_PER_RUN: usize = 1024;

                shuttle::thread::spawn(move || {
                    let mut successful_sends = 0;
                    let mut bank_id = 0;
                    let mut had_successful_send = false;
                    while successful_sends < ITERATIONS_PER_RUN {
                        if sender.try_send(test_record(bank_id, 1)).is_ok() {
                            had_successful_send = true;
                            successful_sends += 1;
                        } else if had_successful_send {
                            bank_id += 1;
                            had_successful_send = false;
                        }
                    }
                });

                // If receiver/sender interaction is buggy there is a race where
                // the receiver can receive a record after shutdown is called.
                // This can cause PoH to panic because it may receive a record
                // for a bank_id that has already been completed.
                let mut current_bank_id = 0;
                receiver.restart(current_bank_id);
                let mut receives = 0;
                while receives < ITERATIONS_PER_RUN {
                    if receiver.is_shutdown() && receiver.is_safe_to_restart() {
                        current_bank_id += 1;
                        receiver.restart(current_bank_id);
                    }

                    if let Ok(record) = receiver.try_recv() {
                        assert!(record.bank_id == current_bank_id, "bank_id mismatch!");
                        receives += 1;
                        receiver.shutdown();
                    }
                }
            },
            NUM_TEST_RUNS,
        )
    }

    #[test]
    fn test_try_recv_not_sent_on_inner_channel_yet() {
        const NUM_TEST_RUNS: usize = 100_000;
        shuttle::check_random(
            || {
                let (sender, mut receiver) = record_channels(false);
                receiver.restart(0);

                {
                    let sender = sender.clone();
                    shuttle::thread::spawn(move || {
                        let _ = sender.try_send(test_record(0, 1));
                    });
                }

                // Snapshot active_senders *before* try_recv
                let safe_to_restart = receiver.is_safe_to_restart();

                // Perform try_recv
                let result = receiver.try_recv();

                // Only fail if it returned None *and* we know there was an active sender at start
                if result.is_err() && !safe_to_restart {
                    panic!("try_recv returned None while a sender was active at start of call");
                }
            },
            NUM_TEST_RUNS,
        )
    }
}
