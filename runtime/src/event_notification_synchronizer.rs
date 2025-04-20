//! Utility to synchronize event notifications among asynchronous notifiers

use std::sync::{atomic::AtomicU64, Condvar, Mutex};

#[derive(Debug, Default)]
pub struct EventNotificationSynchronizer {
    /// The current event sequence number
    event_sequence: AtomicU64,
    /// The processed event sequence number
    processed_event_sequence: Mutex<u64>,
    condvar: Condvar,
}

impl EventNotificationSynchronizer {
    /// Get the next event sequence number.
    /// This function will increment the event sequence number and return the new value.
    pub fn get_new_event_sequence(&self) -> u64 {
        self.event_sequence
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }

    /// Wait for the event to be notified with the expected sequence number.
    /// This function will block until the event sequence is greater than or equal to
    /// the expected sequence number.
    pub fn wait_for_event_processed(&self, expected_sequence: u64) {
        let mut sequence = self.processed_event_sequence.lock().unwrap();
        while *sequence < expected_sequence {
            sequence = self.condvar.wait(sequence).unwrap();
        }
    }

    /// Notify all waiting threads that an event has occurred with the given sequence number.
    /// This function will update the event sequence and notify all waiting threads only the event
    /// sequence is greater than the current sequence.
    pub fn notify_event_processed(&self, sequence: u64) {
        let mut event_sequence = self.processed_event_sequence.lock().unwrap();
        if *event_sequence < sequence {
            *event_sequence = sequence;
            self.condvar.notify_all();
        }
    }

    /// A convient function to wait for the event to be notified with the expected sequence
    /// number and notify all waiting threads with the given sequence number if it
    /// is greater than the current event sequence.
    pub fn wait_and_notify_event_processed(&self, expected_sequence: u64, sequence: u64) {
        let mut event_sequence = self.processed_event_sequence.lock().unwrap();
        while *event_sequence < expected_sequence {
            event_sequence = self.condvar.wait(event_sequence).unwrap();
        }
        if *event_sequence < sequence {
            *event_sequence = sequence;
            self.condvar.notify_all();
        }
    }
}
