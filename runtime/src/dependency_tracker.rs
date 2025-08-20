//! Utility to track dependent work.

use std::sync::{atomic::AtomicU64, Condvar, Mutex};

#[derive(Debug, Default)]
pub struct DependencyTracker {
    /// The current work sequence number
    work_sequence: AtomicU64,
    /// The processed work sequence number, if it is None, no work has been processed
    processed_work_sequence: Mutex<Option<u64>>,
    condvar: Condvar,
}

fn less_than(a: &Option<u64>, b: u64) -> bool {
    a.is_none_or(|a| a < b)
}

impl DependencyTracker {
    /// Acquire the next work sequence number.
    /// The sequence starts from 0 and increments by 1 each time it is called.
    pub fn declare_work(&self) -> u64 {
        self.work_sequence
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1
    }

    /// Notify all waiting threads that a work has occurred with the given sequence number.
    /// This function will update the work sequence and notify all waiting threads only if the work
    /// sequence is greater than the work sequence. Notify a work of sequence number 's' will
    /// implicitly imply that all work with sequence number less than 's' have been processed.
    pub fn mark_this_and_all_previous_work_processed(&self, sequence: u64) {
        let mut work_sequence = self.processed_work_sequence.lock().unwrap();
        if less_than(&work_sequence, sequence) {
            *work_sequence = Some(sequence);
            self.condvar.notify_all();
        }
    }

    /// To wait for the dependency work with 'sequence' to be processed.
    pub fn wait_for_dependency(&self, sequence: u64) {
        if sequence == 0 {
            return; // No need to wait for sequence 0 as real work starts from 1.
        }
        let mut processed_sequence = self.processed_work_sequence.lock().unwrap();
        while less_than(&processed_sequence, sequence) {
            processed_sequence = self.condvar.wait(processed_sequence).unwrap();
        }
    }

    /// Get the current work sequence number.
    pub fn get_current_declared_work(&self) -> u64 {
        self.work_sequence.load(std::sync::atomic::Ordering::SeqCst)
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
    fn test_get_new_work_sequence() {
        let dependency_tracker = DependencyTracker::default();
        assert_eq!(dependency_tracker.declare_work(), 1);
        assert_eq!(dependency_tracker.declare_work(), 2);
        assert_eq!(dependency_tracker.get_current_declared_work(), 2);
    }

    #[test]
    fn test_notify_work_processed() {
        let dependency_tracker = DependencyTracker::default();
        dependency_tracker.mark_this_and_all_previous_work_processed(1);

        let processed_sequence = *dependency_tracker.processed_work_sequence.lock().unwrap();
        assert_eq!(processed_sequence, Some(1));

        // notify a smaller sequence number, should not change the processed sequence
        dependency_tracker.mark_this_and_all_previous_work_processed(0);
        let processed_sequence = *dependency_tracker.processed_work_sequence.lock().unwrap();
        assert_eq!(processed_sequence, Some(1));
        // notify a larger sequence number, should change the processed sequence
        dependency_tracker.mark_this_and_all_previous_work_processed(2);
        let processed_sequence = *dependency_tracker.processed_work_sequence.lock().unwrap();
        assert_eq!(processed_sequence, Some(2));
        // notify the same sequence number, should not change the processed sequence
        dependency_tracker.mark_this_and_all_previous_work_processed(2);
        let processed_sequence = *dependency_tracker.processed_work_sequence.lock().unwrap();
        assert_eq!(processed_sequence, Some(2));
    }

    #[test]
    fn test_wait_and_notify_work_processed() {
        let dependency_tracker = Arc::new(DependencyTracker::default());
        let tracker_clone = Arc::clone(&dependency_tracker);

        let work = dependency_tracker.declare_work();
        assert_eq!(work, 1);
        let work = dependency_tracker.declare_work();
        assert_eq!(work, 2);
        let work_to_wait = dependency_tracker.get_current_declared_work();
        let handle = thread::spawn(move || {
            tracker_clone.wait_for_dependency(work_to_wait);
        });

        thread::sleep(std::time::Duration::from_millis(100));
        dependency_tracker.mark_this_and_all_previous_work_processed(work);
        handle.join().unwrap();

        let processed_sequence = *dependency_tracker.processed_work_sequence.lock().unwrap();
        assert_eq!(processed_sequence, Some(2));
    }
}
