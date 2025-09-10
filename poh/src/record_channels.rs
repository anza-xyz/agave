use {
    crate::poh_recorder::Record,
    crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TryRecvError},
    solana_clock::Slot,
    std::{
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc, Mutex,
        },
        time::Duration,
    },
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

    (
        RecordSender {
            slot_allowed_insertions: slot_allowed_insertions.clone(),
            sender,
            transaction_indexes: transaction_indexes.clone(),
        },
        RecordReceiver {
            slot_allowed_insertions,
            receiver,
            capacity: CAPACITY as u64,
            transaction_indexes,
        },
    )
}

pub enum RecordSenderError {
    /// The channel is full, the record was not sent.
    Full(Record),
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
                return Err(RecordSenderError::Full(record));
            }

            let new_slot_allowed_insertions = SlotAllowedInsertions::encoded_value(
                slot,
                allowed_insertions.wrapping_sub(record.transaction_batches.len() as u64),
            );

            if self
                .slot_allowed_insertions
                .0
                .compare_exchange(
                    current_slot_allowed_insertions,
                    new_slot_allowed_insertions,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                // Send the record over the channel, space has been reserved successfully.
                if let Err(err) = self.sender.try_send(record) {
                    assert!(err.is_disconnected());
                    return Err(RecordSenderError::Disconnected);
                }
                return Ok(transaction_indexes.map(|mut transaction_indexes| {
                    let transaction_starting_index = *transaction_indexes;
                    *transaction_indexes += num_transactions;
                    transaction_starting_index
                }));
            }
        }
    }
}

/// A receiver for receiving [`Record`]s in PohService.
/// The receiver can shutdown the channel, preventing any further sends,
/// and can restart the channel for a new slot, re-enabling sends.
pub struct RecordReceiver {
    capacity: u64,
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

    pub fn is_empty(&self) -> bool {
        self.receiver.is_empty()
    }

    /// Try to receive a record from the channel.
    pub fn try_recv(&self) -> Result<Record, TryRecvError> {
        let record = self.receiver.try_recv()?;
        self.on_received_record(record.transaction_batches.len() as u64);
        Ok(record)
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
    use {super::*, solana_transaction::versioned::VersionedTransaction};

    #[test]
    fn test_record_channels() {
        let (sender, mut receiver) = record_channels(false);

        // Initially shutdown.
        assert!(matches!(
            sender.try_send(Record {
                slot: 0,
                transaction_batches: vec![],
                mixins: vec![],
            }),
            Err(RecordSenderError::Shutdown)
        ));

        // Restart for slot 1.
        receiver.restart(1);

        // Record for slot 0 fails.
        assert!(matches!(
            sender.try_send(Record {
                slot: 0,
                transaction_batches: vec![],
                mixins: vec![],
            }),
            Err(RecordSenderError::InactiveSlot)
        ));

        // Record for slot 1 with 1 batch succeeds.
        assert!(matches!(
            sender.try_send(Record {
                slot: 1,
                transaction_batches: vec![vec![]],
                mixins: vec![],
            }),
            Ok(None)
        ));

        // Record for slot 1 with 1023 batches fails (channel full).
        assert!(matches!(
            sender.try_send(Record {
                slot: 1,
                transaction_batches: vec![vec![]; 1_023],
                mixins: vec![],
            }),
            Err(RecordSenderError::Full(_))
        ));

        // Record for slot 1 with 1023 batches succeeds (channel full).
        assert!(matches!(
            sender.try_send(Record {
                slot: 1,
                transaction_batches: vec![vec![]; 1_022],
                mixins: vec![],
            }),
            Ok(None)
        ));

        // Record for slot 1 with 1 batch fails (channel full).
        assert!(matches!(
            sender.try_send(Record {
                slot: 1,
                transaction_batches: vec![vec![]],
                mixins: vec![],
            }),
            Err(RecordSenderError::Full(_))
        ));

        // Receive 1 record.
        assert!(receiver.try_recv().is_ok());
        assert!(!receiver.is_empty());
        assert!(receiver.try_recv().is_ok());
        assert!(receiver.is_empty());
    }

    #[test]
    fn test_record_channels_track_indexes() {
        let (sender, mut receiver) = record_channels(true);

        // Initially shutdown.
        assert!(matches!(
            sender.try_send(Record {
                slot: 0,
                transaction_batches: vec![],
                mixins: vec![],
            }),
            Err(RecordSenderError::Shutdown)
        ));

        // Restart for slot 1.
        receiver.restart(1);

        // Record for slot 0 fails.
        assert!(matches!(
            sender.try_send(Record {
                slot: 0,
                transaction_batches: vec![],
                mixins: vec![],
            }),
            Err(RecordSenderError::InactiveSlot)
        ));

        // Record for slot 1 with 1 batch succeeds.
        assert!(matches!(
            sender.try_send(Record {
                slot: 1,
                transaction_batches: vec![vec![VersionedTransaction::default()]],
                mixins: vec![],
            }),
            Ok(Some(0))
        ));

        // Record for slot 1 with 2 batches (3 transactions) succeeds.
        assert!(matches!(
            sender.try_send(Record {
                slot: 1,
                transaction_batches: vec![
                    vec![VersionedTransaction::default()],
                    vec![
                        VersionedTransaction::default(),
                        VersionedTransaction::default()
                    ],
                ],
                mixins: vec![],
            }),
            Ok(Some(1))
        ));

        assert!(*sender.transaction_indexes.as_ref().unwrap().lock().unwrap() == 4);
    }
}
