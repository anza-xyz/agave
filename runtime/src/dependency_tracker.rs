//! Utility to track dependent work.

use std::{
    collections::BTreeSet,
    sync::{Condvar, Mutex, atomic::AtomicU64},
};

#[derive(Debug, Default)]
struct ProcessedWork {
    processed_through: u64,
    completed_out_of_order: BTreeSet<u64>,
    closed: bool,
}

#[derive(Debug, Default)]
pub struct DependencyTracker {
    work_id: AtomicU64,
    processed_work: Mutex<ProcessedWork>,
    condvar: Condvar,
}

impl DependencyTracker {
    /// Returns the next work id, starting from 1.
    pub fn declare_work(&self) -> u64 {
        self.work_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1
    }

    /// Marks one work id as processed and advances the contiguous completion watermark.
    pub fn mark_work_processed(&self, work_id: u64) {
        if work_id == 0 {
            return;
        }

        let mut processed_work = self.processed_work.lock().unwrap();
        if processed_work.closed || work_id <= processed_work.processed_through {
            return;
        }

        let Some(next_work_id) = processed_work.processed_through.checked_add(1) else {
            return;
        };
        if work_id == next_work_id {
            processed_work.processed_through = work_id;
            while let Some(next_work_id) = processed_work.processed_through.checked_add(1) {
                if !processed_work.completed_out_of_order.remove(&next_work_id) {
                    break;
                }
                processed_work.processed_through = next_work_id;
            }
            self.condvar.notify_all();
        } else {
            processed_work.completed_out_of_order.insert(work_id);
        }
    }

    /// Waits for all work through `work_id`, or returns false if the tracker closes first.
    #[must_use]
    pub fn wait_for_dependency(&self, work_id: u64) -> bool {
        if work_id == 0 {
            return true;
        }

        let mut processed_work = self.processed_work.lock().unwrap();
        while processed_work.processed_through < work_id && !processed_work.closed {
            processed_work = self.condvar.wait(processed_work).unwrap();
        }
        processed_work.processed_through >= work_id
    }

    /// Closes the tracker and releases its waiters.
    pub fn close(&self) {
        let mut processed_work = self.processed_work.lock().unwrap();
        processed_work.closed = true;
        self.condvar.notify_all();
    }

    /// Returns the latest declared work id.
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
    fn test_get_new_work_id() {
        let dependency_tracker = DependencyTracker::default();
        assert_eq!(dependency_tracker.declare_work(), 1);
        assert_eq!(dependency_tracker.declare_work(), 2);
        assert_eq!(dependency_tracker.get_current_declared_work(), 2);
    }

    #[test]
    fn test_mark_work_processed_out_of_order() {
        let dependency_tracker = DependencyTracker::default();
        let first_work = dependency_tracker.declare_work();
        let second_work = dependency_tracker.declare_work();

        dependency_tracker.mark_work_processed(second_work);
        {
            let processed_work = dependency_tracker.processed_work.lock().unwrap();
            assert_eq!(processed_work.processed_through, 0);
            assert_eq!(
                processed_work.completed_out_of_order,
                BTreeSet::from([second_work])
            );
        }

        dependency_tracker.mark_work_processed(first_work);
        {
            let processed_work = dependency_tracker.processed_work.lock().unwrap();
            assert_eq!(processed_work.processed_through, second_work);
            assert!(processed_work.completed_out_of_order.is_empty());
        }

        dependency_tracker.mark_work_processed(first_work);
        dependency_tracker.mark_work_processed(0);
        assert_eq!(
            dependency_tracker
                .processed_work
                .lock()
                .unwrap()
                .processed_through,
            second_work
        );
    }

    #[test]
    fn test_wait_for_dependency_and_close() {
        let dependency_tracker = Arc::new(DependencyTracker::default());
        let tracker_clone = Arc::clone(&dependency_tracker);

        let first_work = dependency_tracker.declare_work();
        let second_work = dependency_tracker.declare_work();
        dependency_tracker.mark_work_processed(second_work);
        let handle = thread::spawn(move || tracker_clone.wait_for_dependency(second_work));

        dependency_tracker.mark_work_processed(first_work);
        assert!(handle.join().unwrap());

        let dependency_tracker = Arc::new(DependencyTracker::default());
        let tracker_clone = Arc::clone(&dependency_tracker);
        let work = dependency_tracker.declare_work();
        let handle = thread::spawn(move || tracker_clone.wait_for_dependency(work));

        dependency_tracker.close();
        assert!(!handle.join().unwrap());
        assert!(dependency_tracker.wait_for_dependency(0));
    }
}
