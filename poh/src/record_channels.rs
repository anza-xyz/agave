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
    solana_clock::Slot,
    std::time::Duration,
};

/// Create a channel pair for communicating [`Record`]s.
/// Transaction processing threads (workers/vote thread) send records, and
/// PohService receives them.
///
/// The receiver can shutdown the channel, preventing any further sends,
/// and can restart the channel for a new slot, re-enabling sends.
/// The sender does not wait for the receiver to pick up records, and will return
/// immediately if the channel is full, shutdown, or if the slot has changed.
///
/// The channel has a bounded capacity based on the maximum number of allowed
/// insertions at a given time. This is for guaranteeing that once shutdown the
/// service can always process all sent records correctly without dropping any
/// i.e. once sent records can be guaranteed to be recorded.
pub fn record_channels(track_transaction_indexes: bool) -> (RecordSender, RecordReceiver) {
    const CAPACITY: usize = SlotAllowedInsertions::MAX_ALLOWED_INSERTIONS as usize;
    let (sender, receiver) = bounded(CAPACITY);

    // Begin in a shutdown state.
    let slot_allowed_insertions = SlotAllowedInsertions::new_shutdown();
    let transaction_indexes = if track_transaction_indexes {
        Some(Arc::new(Mutex::new(0)))
    } else {
        None
    };

    let active_senders = Arc::new(AtomicU64::new(0));
    (
        RecordSender {
            active_senders: active_senders.clone(),
            slot_allowed_insertions: slot_allowed_insertions.clone(),
            sender,
            transaction_indexes: transaction_indexes.clone(),
        },
        RecordReceiver {
            active_senders,
            slot_allowed_insertions,
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
    /// send records for this slot anymore.
    Shutdown,
    /// The record's slot does not match the current slot of the channel.
    InactiveSlot,
    /// The receiver has been dropped, the channel is disconnected.
    Disconnected,
}

/// A sender for sending [`Record`]s to PohService.
/// The sender does not wait for service to pick up the records, and will return
/// immediately if the channel is full, shutdown, or if the slot has changed.
#[derive(Clone, Debug)]
pub struct RecordSender {
    /// Used to track active senders for the current slot. Used so that the receiver
    /// side can determine that no more sends are in-flight while shutting down.
    active_senders: Arc<AtomicU64>,
    slot_allowed_insertions: SlotAllowedInsertions,
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

            // Get the current slot and allowed insertions.
            // If the number of allowed insertions is less than the number of
            // batches, the channel is full - just return immediately.
            // If the `record`'s slot is different from the current slot,
            // return immediately.
            let current_slot_allowed_insertions =
                self.slot_allowed_insertions.0.load(Ordering::Acquire);
            let (slot, allowed_insertions) = (
                SlotAllowedInsertions::slot(current_slot_allowed_insertions),
                SlotAllowedInsertions::allowed_insertions(current_slot_allowed_insertions),
            );

            if slot == SlotAllowedInsertions::DISABLED_SLOT {
                return Err(RecordSenderError::Shutdown);
            }
            if slot != record.slot {
                return Err(RecordSenderError::InactiveSlot);
            }
            if allowed_insertions < record.transaction_batches.len() as u64 {
                return Err(RecordSenderError::Full);
            }

            let new_slot_allowed_insertions = SlotAllowedInsertions::encoded_value(
                slot,
                allowed_insertions.wrapping_sub(record.transaction_batches.len() as u64),
            );

            // Increment this before CAS so the receiver can see this send is in-flight.
            self.active_senders.fetch_add(1, Ordering::AcqRel);

            if self
                .slot_allowed_insertions
                .0
                .compare_exchange(
                    current_slot_allowed_insertions,
                    new_slot_allowed_insertions,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                // Failed to reserve space, decrement active senders and try again.
                self.active_senders.fetch_sub(1, Ordering::AcqRel);
                continue;
            }

            match self.sender.try_send(record) {
                Ok(_) => {
                    self.active_senders.fetch_sub(1, Ordering::AcqRel);
                    return Ok(transaction_indexes.map(|mut transaction_indexes| {
                        let transaction_starting_index = *transaction_indexes;
                        *transaction_indexes += num_transactions;
                        transaction_starting_index
                    }));
                }
                Err(err) => {
                    assert!(err.is_disconnected());
                    self.active_senders.fetch_sub(1, Ordering::AcqRel);
                    return Err(RecordSenderError::Disconnected);
                }
            }
        }
    }
}

/// A receiver for receiving [`Record`]s in PohService.
/// The receiver can shutdown the channel, preventing any further sends,
/// and can restart the channel for a new slot, re-enabling sends.
pub struct RecordReceiver {
    capacity: u64,
    active_senders: Arc<AtomicU64>,
    slot_allowed_insertions: SlotAllowedInsertions,
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
        self.slot_allowed_insertions.shutdown();
    }

    /// Check if the channel is shutdown.
    pub(crate) fn is_shutdown(&self) -> bool {
        SlotAllowedInsertions::slot(self.slot_allowed_insertions.0.load(Ordering::Acquire))
            == SlotAllowedInsertions::DISABLED_SLOT
    }

    /// Re-enable the channel after a shutdown.
    pub fn restart(&mut self, slot: Slot) {
        assert!(slot <= SlotAllowedInsertions::MAX_SLOT);
        assert!(self.receiver.is_empty()); // Should be empty before restarting.

        // Reset transaction indexes if tracking them - BEFORE allowing new insertions.
        let transaction_indexes_lock =
            self.transaction_indexes
                .as_ref()
                .map(|transaction_indexes| {
                    let mut lock = transaction_indexes.lock().unwrap();
                    *lock = 0;
                    lock
                });

        self.slot_allowed_insertions.0.store(
            SlotAllowedInsertions::encoded_value(slot, self.capacity),
            Ordering::Release,
        );

        // Drop lock AFTER allowing new insertions. This makes any sends grabbing locks
        // wait until after the slot has been changed. Meaning the CAS in try_send
        // will always succeed, if passing previous checks.
        drop(transaction_indexes_lock);
    }

    /// Drain all available records from the channel with `try_recv` loop.
    pub fn drain(&self) -> impl Iterator<Item = Record> + '_ {
        core::iter::from_fn(|| self.try_recv().ok())
    }

    /// Channel is empty and there are no active threads attempting to send.
    pub fn is_safe_to_restart(&self) -> bool {
        // The order here is important. active_senders must be checked first.
        // If checked after is_empty, we could have a race:
        // 1) sender has not sent yet, active_senders = 1. is_empty = true.
        // 2) sender sends, decrements active_senders = 0.
        // 3) receiver checks active_senders == 0 && is_empty == true,
        //    thinks the channel is empty with no active senders, but there is
        //    actually a record in the channel now!
        self.active_senders.load(Ordering::Acquire) == 0 && self.receiver.is_empty()
    }

    /// Try to receive a record from the channel.
    pub fn try_recv(&self) -> Result<Record, TryRecvError> {
        // In order to avoid returning None when there was an active sender
        // we load `active_senders` prior to try_recv.
        let mut sender_active = self.active_senders.load(Ordering::Acquire) > 0;

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
                        sender_active = self.active_senders.load(Ordering::Acquire) > 0;
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
        self.slot_allowed_insertions
            .0
            .fetch_add(num_batches, Ordering::AcqRel);
    }
}

/// Encoded u64 where the upper 54 bits are the slot and the lower 10 bits are
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
struct SlotAllowedInsertions(Arc<AtomicU64>);

impl SlotAllowedInsertions {
    const NUM_BITS: u64 = 64;
    /// Number of bits used to track allowed insertions.
    const ALLOWED_INSERTIONS_BITS: u64 = 10;
    const SLOT_BITS: u64 = Self::NUM_BITS - Self::ALLOWED_INSERTIONS_BITS;

    const DISABLED_SLOT: Slot = (1 << Self::SLOT_BITS) - 1;
    const MAX_SLOT: Slot = Self::DISABLED_SLOT - 1;
    const MAX_ALLOWED_INSERTIONS: u64 = (1 << Self::ALLOWED_INSERTIONS_BITS) - 1;

    const SHUTDOWN: u64 = Self::encoded_value(Self::DISABLED_SLOT, 0);

    /// Create a new `SlotAllowedInsertions` with state consistent with a
    /// shutdown state:
    /// - slot = `DISABLED_SLOT`
    /// - allowed_insertions = 0
    fn new_shutdown() -> Self {
        Self(Arc::new(AtomicU64::new(Self::SHUTDOWN)))
    }

    /// Shutdown the channel immediately.
    fn shutdown(&self) {
        self.0.store(Self::SHUTDOWN, Ordering::Release);
    }

    const fn encoded_value(slot: Slot, allowed_insertions: u64) -> u64 {
        assert!(slot <= Self::DISABLED_SLOT);
        assert!(allowed_insertions <= Self::MAX_ALLOWED_INSERTIONS);
        (slot << Self::ALLOWED_INSERTIONS_BITS) | allowed_insertions
    }

    /// The current slot, or `DISABLED_SLOT` if shutdown.
    fn slot(value: u64) -> Slot {
        (value >> Self::ALLOWED_INSERTIONS_BITS) & Self::DISABLED_SLOT
    }

    /// How many insertions/sends are allowed at this time.
    fn allowed_insertions(value: u64) -> u64 {
        value & Self::MAX_ALLOWED_INSERTIONS
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_hash::Hash, solana_transaction::versioned::VersionedTransaction};

    pub(super) fn test_record(slot: Slot, num_batches: usize) -> Record {
        Record {
            slot,
            transaction_batches: (0..num_batches)
                .map(|_| vec![VersionedTransaction::default()])
                .collect(),
            mixins: (0..num_batches).map(|_| Hash::default()).collect(),
        }
    }

    #[test]
    fn test_record_channels() {
        let (sender, mut receiver) = record_channels(false);

        // Initially shutdown.
        assert!(matches!(
            sender.try_send(test_record(0, 1)),
            Err(RecordSenderError::Shutdown)
        ));

        // Restart for slot 1.
        receiver.restart(1);

        // Record for slot 0 fails.
        assert!(matches!(
            sender.try_send(test_record(0, 1)),
            Err(RecordSenderError::InactiveSlot)
        ));

        // Record for slot 1 with 1 batch succeeds.
        assert!(matches!(sender.try_send(test_record(1, 1)), Ok(None)));

        // Record for slot 1 with 1023 batches fails (channel full).
        assert!(matches!(
            sender.try_send(test_record(1, 1023)),
            Err(RecordSenderError::Full)
        ));

        // Record for slot 1 with 1022 batches succeeds (channel now full).
        assert!(matches!(sender.try_send(test_record(1, 1022)), Ok(None)));

        // Record for slot 1 with 1 batch fails (channel full).
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

        // Restart for slot 1.
        receiver.restart(1);

        // Record for slot 0 fails.
        assert!(matches!(
            sender.try_send(test_record(0, 1)),
            Err(RecordSenderError::InactiveSlot)
        ));

        // Record for slot 1 with 1 batch succeeds.
        assert!(matches!(sender.try_send(test_record(1, 1)), Ok(Some(0))));

        // Record for slot 1 with 2 batches (3 transactions) succeeds.
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
                    let mut slot = 0;
                    let mut had_successful_send = false;
                    while successful_sends < ITERATIONS_PER_RUN {
                        if sender.try_send(test_record(slot, 1)).is_ok() {
                            had_successful_send = true;
                            successful_sends += 1;
                        } else if had_successful_send {
                            slot += 1;
                            had_successful_send = false;
                        }
                    }
                });

                // If receiver/sender interaction is buggy there is a race where
                // the receiver can receive a record after shutdown is called.
                // This can cause PoH to panic because it may receive a record
                // for a slot that has already been completed.
                let mut current_slot = 0;
                receiver.restart(current_slot);
                let mut receives = 0;
                while receives < ITERATIONS_PER_RUN {
                    if receiver.is_shutdown() && receiver.is_safe_to_restart() {
                        current_slot += 1;
                        receiver.restart(current_slot);
                    }

                    if let Ok(record) = receiver.try_recv() {
                        assert!(record.slot == current_slot, "slot mismatch!");
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
                let active_at_start = sender.active_senders.load(Ordering::Acquire);

                // Perform try_recv
                let result = receiver.try_recv();

                // Only fail if it returned None *and* we know there was an active sender at start
                if result.is_err() && active_at_start > 0 {
                    panic!(
                        "try_recv returned None while a sender was active at start of call \
                         (active_senders={})",
                        active_at_start
                    );
                }
            },
            NUM_TEST_RUNS,
        )
    }
}
