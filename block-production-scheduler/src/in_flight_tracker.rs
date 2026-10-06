#[derive(Default)]
pub(crate) struct WorkerLoad {
    pub(crate) batches: usize,
    pub(crate) transactions: usize,
    pub(crate) cost_units: u64,
}

/// Execution work published to workers whose responses have not yet been handled.
pub(crate) struct InFlightTracker {
    workers: Box<[WorkerLoad]>,
}

impl InFlightTracker {
    pub(crate) fn new(num_workers: usize) -> Self {
        Self {
            workers: (0..num_workers).map(|_| WorkerLoad::default()).collect(),
        }
    }

    /// # Panics
    /// Panics if `worker` is outside the configured worker range.
    pub(crate) fn worker_load(&self, worker: usize) -> &WorkerLoad {
        &self.workers[worker]
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.workers.iter().all(|worker| worker.batches == 0)
    }

    /// Records a successfully published batch.
    ///
    /// # Panics
    /// Panics if `worker` is outside the configured worker range.
    pub(crate) fn track_batch(&mut self, worker: usize, transactions: usize, cost_units: u64) {
        let load = &mut self.workers[worker];
        load.batches = load.batches.wrapping_add(1);
        load.transactions = load.transactions.wrapping_add(transactions);
        load.cost_units = load.cost_units.wrapping_add(cost_units);
    }

    /// Removes a completed batch using its original transaction count and estimated cost,
    /// including when execution failed. Actual costs are accounted for separately for pacing.
    ///
    /// # Panics
    /// Panics if `worker` is outside the configured worker range. In debug builds, also panics
    /// if no batch is outstanding or the completed transaction count or cost exceeds the load.
    pub(crate) fn complete_batch(&mut self, worker: usize, transactions: usize, cost_units: u64) {
        let load = &mut self.workers[worker];
        debug_assert!(load.batches > 0);
        debug_assert!(load.transactions >= transactions);
        debug_assert!(load.cost_units >= cost_units);
        load.batches = load.batches.wrapping_sub(1);
        load.transactions = load.transactions.wrapping_sub(transactions);
        load.cost_units = load.cost_units.wrapping_sub(cost_units);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_worker_load_until_completion() {
        let mut tracker = InFlightTracker::new(2);
        assert!(tracker.is_empty());
        tracker.track_batch(0, 2, 100);
        tracker.track_batch(0, 1, 50);
        tracker.track_batch(1, 1, 0);
        assert_eq!(tracker.worker_load(0).batches, 2);
        assert_eq!(tracker.worker_load(1).batches, 1);
        assert_eq!(tracker.worker_load(0).transactions, 3);
        assert_eq!(tracker.worker_load(0).cost_units, 150);

        tracker.complete_batch(0, 2, 100);
        assert_eq!(tracker.worker_load(0).batches, 1);
        assert_eq!(tracker.worker_load(0).transactions, 1);
        assert_eq!(tracker.worker_load(0).cost_units, 50);
        tracker.complete_batch(0, 1, 50);
        assert_eq!(tracker.worker_load(0).transactions, 0);
        assert_eq!(tracker.worker_load(0).cost_units, 0);
        assert!(!tracker.is_empty());

        tracker.complete_batch(1, 1, 0);
        assert_eq!(tracker.worker_load(1).transactions, 0);
        assert_eq!(tracker.worker_load(1).cost_units, 0);
        assert!(tracker.is_empty());
    }
}
