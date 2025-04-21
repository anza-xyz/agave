//! Utility to synchronize event notifications among asynchronous notifiers

use std::sync::{atomic::AtomicU64, Condvar, Mutex};

#[derive(Debug, Default)]
pub struct EventNotificationSynchronizer {
    /// The current event sequence number
    event_sequence: AtomicU64,
    /// The processed event sequence number, if it is None, no event has been processed
    processed_event_sequence: Mutex<Option<u64>>,
    condvar: Condvar,
}

fn less_than(a: &Option<u64>, b: u64) -> bool {
    a.is_none_or(|a| a < b)
}

impl EventNotificationSynchronizer {
    /// Get the next event sequence number.
    /// This function will increment the event sequence number and return the
    /// the previously stored value as the new value.
    /// The sequence starts from 0 and increments by 1 each time it is called.
    pub fn get_new_event_sequence(&self) -> u64 {
        self.event_sequence
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }

    /// Wait for the event to be notified with the expected sequence number.
    /// This function will block until the event sequence is greater than or equal to
    /// the expected sequence number.
    pub fn wait_for_event_processed(&self, expected_sequence: u64) {
        let mut sequence = self.processed_event_sequence.lock().unwrap();
        while less_than(&sequence, expected_sequence) {
            sequence = self.condvar.wait(sequence).unwrap();
        }
    }

    /// Notify all waiting threads that an event has occurred with the given sequence number.
    /// This function will update the event sequence and notify all waiting threads only the event
    /// sequence is greater than the current sequence.
    pub fn notify_event_processed(&self, sequence: u64) {
        let mut event_sequence = self.processed_event_sequence.lock().unwrap();
        if less_than(&event_sequence, sequence) {
            *event_sequence = Some(sequence);
            self.condvar.notify_all();
        }
    }

    /// A convient function to wait for the predecessor event sequence to be notified
    /// and notify all waiting threads awaiting this sequence the input event sequence.
    /// The predecessor event sequence number is the sequence number minus 1.
    /// If the sequence number is 0, it will wait for None as it is the first event.
    pub fn wait_and_notify_event_processed(&self, sequence: u64) {
        let mut processed_sequence = self.processed_event_sequence.lock().unwrap();
        if sequence > 0 {
            let expected_sequence = sequence - 1;
            while less_than(&processed_sequence, expected_sequence) {
                processed_sequence = self.condvar.wait(processed_sequence).unwrap();
            }
        }

        if less_than(&processed_sequence, sequence) {
            *processed_sequence = Some(sequence);
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
    fn test_less_than() {
        assert!(less_than(&None, 0));
        assert!(less_than(&Some(0), 1));
        assert!(!less_than(&Some(1), 1));
        assert!(!less_than(&Some(2), 1));
    }

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
        assert_eq!(processed_sequence, Some(1));

        // notify a smaller sequence number, should not change the processed sequence
        synchronizer.notify_event_processed(0);
        let processed_sequence = *synchronizer.processed_event_sequence.lock().unwrap();
        assert_eq!(processed_sequence, Some(1));
        // notify a larger sequence number, should change the processed sequence
        synchronizer.notify_event_processed(2);
        let processed_sequence = *synchronizer.processed_event_sequence.lock().unwrap();
        assert_eq!(processed_sequence, Some(2));
        // notify the same sequence number, should not change the processed sequence
        synchronizer.notify_event_processed(2);
        let processed_sequence = *synchronizer.processed_event_sequence.lock().unwrap();
        assert_eq!(processed_sequence, Some(2));
    }

    #[test]
    fn test_wait_and_notify_event_processed() {
        let synchronizer = Arc::new(EventNotificationSynchronizer::default());
        let synchronizer_clone = Arc::clone(&synchronizer);

        let handle = thread::spawn(move || {
            synchronizer_clone.wait_and_notify_event_processed(2);
        });

        thread::sleep(std::time::Duration::from_millis(100));
        synchronizer.notify_event_processed(1);
        handle.join().unwrap();

        let processed_sequence = *synchronizer.processed_event_sequence.lock().unwrap();
        assert_eq!(processed_sequence, Some(2));
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
                synchronizer_clone.wait_and_notify_event_processed(i + 1);
            }));
        }

        // Notify the first event
        thread::sleep(std::time::Duration::from_millis(50));
        synchronizer.notify_event_processed(0);

        for handle in handles {
            handle.join().unwrap();
        }

        let processed_sequence = *synchronizer.processed_event_sequence.lock().unwrap();
        assert_eq!(processed_sequence, Some(5));
    }
}
