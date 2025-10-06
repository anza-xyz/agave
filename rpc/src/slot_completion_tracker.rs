//! Slot completion tracking for ensuring transaction status consistency
//! when transaction status service is parallelized.
//!
//! This module provides thread-safe tracking of slot completion to prevent
//! race conditions where RPC queries might see incomplete transaction data.
//! It ensures that `max_complete_transaction_status_slot` only advances when
//! all batches for consecutive slots have been written to the blockstore.

use {
    dashmap::DashMap,
    solana_clock::Slot,
    std::sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
};

/// Callback trait for slot completion notifications
pub trait SlotCompletionCallback: Send + Sync {
    fn on_slot_complete(&self, slot: Slot);
}

/// Thread-safe tracker for slot completion with consecutive ordering guarantees
pub struct SlotCompletionTracker {
    pending_batches: DashMap<Slot, AtomicUsize>, // slot -> remaining batch count
    completed_slots: DashMap<Slot, ()>,          // completed slots waiting for consecutive update
    max_complete_transaction_status_slot: Arc<AtomicU64>,
}

impl SlotCompletionTracker {
    /// Creates a new slot completion tracker
    pub fn new(max_complete_transaction_status_slot: Arc<AtomicU64>) -> Self {
        Self {
            pending_batches: DashMap::new(),
            completed_slots: DashMap::new(),
            max_complete_transaction_status_slot,
        }
    }

    /// Registers the start of a batch for the given slot
    ///
    /// This increments the pending batch count for the slot, ensuring
    /// that the slot won't be marked as complete until all batches finish.
    pub fn start_batch(&self, slot: Slot) {
        self.pending_batches
            .entry(slot)
            .or_insert_with(|| AtomicUsize::new(0))
            .fetch_add(1, Ordering::SeqCst);
    }

    /// Marks a batch as complete for the given slot
    ///
    /// When the last batch for a slot completes, this triggers an attempt
    /// to update the max complete slot to maintain consecutive ordering.
    pub fn complete_batch(&self, slot: Slot) {
        let is_last_batch = if let Some(count_ref) = self.pending_batches.get(&slot) {
            count_ref.fetch_sub(1, Ordering::SeqCst) == 1
        } else {
            false
        };

        if is_last_batch {
            // Last batch for this slot. Remove it from pending.
            self.pending_batches.remove(&slot);
            self.completed_slots.insert(slot, ());

            // Update max_complete_transaction_status_slot to highest consecutive slot
            self.update_consecutive_max();
        }
    }

    /// Updates the max complete slot to the highest consecutive completed slot
    ///
    /// This ensures that max_complete_transaction_status_slot only advances
    /// when there are no gaps in the completed slot sequence, preventing
    /// RPC queries from trying to access non-existent transaction data.
    fn update_consecutive_max(&self) {
        let current_max = self
            .max_complete_transaction_status_slot
            .load(Ordering::SeqCst);
        let mut next_slot = current_max + 1;

        // Find consecutive completed slots starting from current_max + 1
        while self.completed_slots.contains_key(&next_slot) {
            self.completed_slots.remove(&next_slot);
            next_slot += 1;
        }

        // Update to the highest consecutive slot
        if next_slot > current_max + 1 {
            self.max_complete_transaction_status_slot
                .store(next_slot - 1, Ordering::SeqCst);
        }
    }
}

/// Implementation of SlotCompletionCallback that uses SlotCompletionTracker
pub struct SlotCompletionCallbackImpl {
    tracker: Arc<SlotCompletionTracker>,
}

impl SlotCompletionCallbackImpl {
    pub fn new(tracker: Arc<SlotCompletionTracker>) -> Self {
        Self { tracker }
    }
}

impl SlotCompletionCallback for SlotCompletionCallbackImpl {
    fn on_slot_complete(&self, slot: Slot) {
        self.tracker.complete_batch(slot);
    }
}

#[cfg(test)]
mod tests {
    use {super::*, std::sync::atomic::AtomicU64};

    #[test]
    fn test_consecutive_slot_completion() {
        let max_slot = Arc::new(AtomicU64::new(100));
        let tracker = SlotCompletionTracker::new(Arc::clone(&max_slot));

        // Complete slots out of order: 104, 101, 102, 103
        tracker.start_batch(101);
        tracker.start_batch(102);
        tracker.start_batch(103);
        tracker.start_batch(104);

        // Complete slot 104 first - should not update max
        tracker.complete_batch(104);
        assert_eq!(max_slot.load(Ordering::SeqCst), 100);

        // Complete slot 101 - should update max to 101
        tracker.complete_batch(101);
        assert_eq!(max_slot.load(Ordering::SeqCst), 101);

        // Complete slot 102 - should update max to 102
        tracker.complete_batch(102);
        assert_eq!(max_slot.load(Ordering::SeqCst), 102);

        // Complete slot 103 - should update max to 104 (all consecutive)
        tracker.complete_batch(103);
        assert_eq!(max_slot.load(Ordering::SeqCst), 104);

        // After all batches are completed, both trackers should be empty
        assert!(tracker.pending_batches.is_empty());
        assert!(tracker.completed_slots.is_empty());
    }

    #[test]
    fn test_multiple_batches_per_slot() {
        let max_slot = Arc::new(AtomicU64::new(0));
        let tracker = SlotCompletionTracker::new(Arc::clone(&max_slot));

        // Start 3 batches for slot 1
        tracker.start_batch(1);
        tracker.start_batch(1);
        tracker.start_batch(1);

        // Complete first two batches - should not update max
        tracker.complete_batch(1);
        assert_eq!(max_slot.load(Ordering::SeqCst), 0);
        tracker.complete_batch(1);
        assert_eq!(max_slot.load(Ordering::SeqCst), 0);

        // Complete last batch - should update max to 1
        tracker.complete_batch(1);
        assert_eq!(max_slot.load(Ordering::SeqCst), 1);

        // After all batches are completed, both trackers should be empty
        assert!(tracker.pending_batches.is_empty());
        assert!(tracker.completed_slots.is_empty());
    }
}
