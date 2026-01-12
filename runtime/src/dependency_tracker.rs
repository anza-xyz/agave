//! Utility to track dependent work.

use std::sync::{Condvar, Mutex, atomic::AtomicU64};

#[derive(Debug, Default)]
pub struct DependencyTracker {
    /// The current work id
    work_id: AtomicU64,
    /// The processed work id, if it is None, no work has been processed
    processed_work_id: Mutex<Option<u64>>,
    condvar: Condvar,
}

fn less_than(a: &Option<u64>, b: u64) -> bool {
    a.is_none_or(|a| a < b)
}

impl DependencyTracker {
    /// Acquire the next work id number.
    /// The work id starts from 0 and increments by 1 each time it is called.
    pub fn declare_work(&self) -> u64 {
        self.work_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1
    }

    /// Notify all waiting threads that a work has been processed with the given work id.
    /// This function will update the processed work id and notify all waiting threads only if the work
    /// id is greater than the procsessed work id. Notify a work of id number 's' will
    /// implicitly imply that all work with id number less than 's' have been processed.
    pub fn mark_this_and_all_previous_work_processed(&self, work_id: u64) {
        let mut processed_work_id = self.processed_work_id.lock().unwrap();
        if less_than(&processed_work_id, work_id) {
            *processed_work_id = Some(work_id);
            self.condvar.notify_all();
        }
    }

    /// To wait for the dependency work with 'work_id' to be processed.
    pub fn wait_for_dependency(&self, work_id: u64) {
        if work_id == 0 {
            return; // No need to wait for work id 0 as real work starts from 1.
        }
        let mut processed_work_id = self.processed_work_id.lock().unwrap();
        while less_than(&processed_work_id, work_id) {
            processed_work_id = self.condvar.wait(processed_work_id).unwrap();
        }
    }

    /// Get the current work id number.
    pub fn get_current_declared_work(&self) -> u64 {
        self.work_id.load(std::sync::atomic::Ordering::SeqCst)
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
    fn test_get_new_work_id() {
        let dependency_tracker = DependencyTracker::default();
        assert_eq!(dependency_tracker.declare_work(), 1);
        assert_eq!(dependency_tracker.declare_work(), 2);
        assert_eq!(dependency_tracker.get_current_declared_work(), 2);
    }

    #[test]
    fn test_notify_work_processed() {
        let dependency_tracker = DependencyTracker::default();
        dependency_tracker.mark_this_and_all_previous_work_processed(1);

        let processed_work_id = *dependency_tracker.processed_work_id.lock().unwrap();
        assert_eq!(processed_work_id, Some(1));

        // notify a smaller work id number, should not change the processed work id
        dependency_tracker.mark_this_and_all_previous_work_processed(0);
        let processed_work_id = *dependency_tracker.processed_work_id.lock().unwrap();
        assert_eq!(processed_work_id, Some(1));
        // notify a larger work id number, should change the processed work id
        dependency_tracker.mark_this_and_all_previous_work_processed(2);
        let processed_work_id = *dependency_tracker.processed_work_id.lock().unwrap();
        assert_eq!(processed_work_id, Some(2));
        // notify the same work id number, should not change the processed work id
        dependency_tracker.mark_this_and_all_previous_work_processed(2);
        let processed_work_id = *dependency_tracker.processed_work_id.lock().unwrap();
        assert_eq!(processed_work_id, Some(2));
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

        let processed_work_id = *dependency_tracker.processed_work_id.lock().unwrap();
        assert_eq!(processed_work_id, Some(2));
    }
}
