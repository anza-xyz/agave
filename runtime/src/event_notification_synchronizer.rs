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

#[cfg(test)]
mod tests {
    use {
        super::*,
        std::{sync::Arc, thread},
    };

    #[test]
    fn test_get_new_event_sequence() {
        let synchronizer = EventNotificationSynchronizer::default();
        assert_eq!(synchronizer.get_new_event_sequence(), 0);
        assert_eq!(synchronizer.get_new_event_sequence(), 1);
    }

    #[test]
    fn test_wait_for_event_processed() {
        let synchronizer = Arc::new(EventNotificationSynchronizer::default());
        let synchronizer_clone = Arc::clone(&synchronizer);

        let handle = thread::spawn(move || {
            synchronizer_clone.wait_for_event_processed(1);
        });

        thread::sleep(std::time::Duration::from_millis(100));
        synchronizer.notify_event_processed(1);
        handle.join().unwrap();
    }

    #[test]
    fn test_notify_event_processed() {
        let synchronizer = EventNotificationSynchronizer::default();
        synchronizer.notify_event_processed(1);

        let processed_sequence = *synchronizer.processed_event_sequence.lock().unwrap();
        assert_eq!(processed_sequence, 1);

        // notify a smaller sequence number, should not change the processed sequence
        synchronizer.notify_event_processed(0);
        let processed_sequence = *synchronizer.processed_event_sequence.lock().unwrap();
        assert_eq!(processed_sequence, 1);
        // notify a larger sequence number, should change the processed sequence
        synchronizer.notify_event_processed(2);
        let processed_sequence = *synchronizer.processed_event_sequence.lock().unwrap();
        assert_eq!(processed_sequence, 2);
        // notify the same sequence number, should not change the processed sequence
        synchronizer.notify_event_processed(2);
        let processed_sequence = *synchronizer.processed_event_sequence.lock().unwrap();
        assert_eq!(processed_sequence, 2);
    }

    #[test]
    fn test_wait_and_notify_event_processed() {
        let synchronizer = Arc::new(EventNotificationSynchronizer::default());
        let synchronizer_clone = Arc::clone(&synchronizer);

        let handle = thread::spawn(move || {
            synchronizer_clone.wait_and_notify_event_processed(1, 2);
        });

        thread::sleep(std::time::Duration::from_millis(100));
        synchronizer.notify_event_processed(1);
        handle.join().unwrap();

        let processed_sequence = *synchronizer.processed_event_sequence.lock().unwrap();
        assert_eq!(processed_sequence, 2);
    }

    #[test]
    fn test_multiple_threads_wait_and_notify() {
        let synchronizer = Arc::new(EventNotificationSynchronizer::default());
        let mut handles = vec![];

        // simulating cascading event, the first thread wait for the event with sequence 0, and it then signal
        // the next thread with sequence 1, and so on
        for i in 0..5 {
            let synchronizer_clone = Arc::clone(&synchronizer);
            handles.push(thread::spawn(move || {
                synchronizer_clone.wait_and_notify_event_processed(i, i + 1);
            }));
        }

        // Notify the first event
        thread::sleep(std::time::Duration::from_millis(50));
        synchronizer.notify_event_processed(0);

        for handle in handles {
            handle.join().unwrap();
        }

        let processed_sequence = *synchronizer.processed_event_sequence.lock().unwrap();
        assert_eq!(processed_sequence, 5);
    }
}
