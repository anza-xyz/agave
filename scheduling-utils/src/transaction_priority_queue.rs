use std::{
    collections::{BTreeSet, btree_set::Range},
    iter::Rev,
    ops::Bound,
};

/// A unique transaction identifier paired with its scheduling priority.
///
/// IDs are `usize` so both Slab indices and future shared-memory allocation offsets can use the
/// same priority queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TransactionPriorityId {
    pub priority: u64,
    pub id: usize,
}

impl TransactionPriorityId {
    pub const fn new(priority: u64, id: usize) -> Self {
        Self { priority, id }
    }
}

/// Priority ordering, held retries, and resumable scans for transaction IDs.
pub struct TransactionPriorityQueue {
    capacity: usize,
    queued: BTreeSet<TransactionPriorityId>,
    held: Vec<TransactionPriorityId>,
}

impl TransactionPriorityQueue {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            queued: BTreeSet::new(),
            held: Vec::with_capacity(capacity),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.queued.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queued.is_empty()
    }

    pub fn pop_highest(&mut self) -> Option<TransactionPriorityId> {
        self.queued.pop_last()
    }

    pub fn remove(&mut self, id: &TransactionPriorityId) -> bool {
        self.queued.remove(id)
    }

    pub fn min_max_priority(&self) -> Option<(u64, u64)> {
        Some((self.queued.first()?.priority, self.queued.last()?.priority))
    }

    /// Iterates in descending priority order, resuming strictly below `cursor` when present.
    pub fn descending_from(
        &self,
        cursor: Option<&TransactionPriorityId>,
    ) -> Rev<Range<'_, TransactionPriorityId>> {
        match cursor {
            None => self.queued.range(..).rev(),
            Some(cursor) => self
                .queued
                .range((Bound::Unbounded, Bound::Excluded(cursor)))
                .rev(),
        }
    }

    pub fn hold(&mut self, id: TransactionPriorityId) {
        self.held.push(id);
    }

    /// Queues IDs and evicts the `num_to_evict` lowest-priority queued IDs.
    ///
    /// The queue has no knowledge of the owner's capacity policy, so the caller determines how many
    /// IDs to evict. The callback removes evicted state from its owner.
    ///
    /// # Panics
    ///
    /// Panics if `num_to_evict` exceeds the number of queued IDs after inserting `ids`.
    pub fn push(
        &mut self,
        ids: impl Iterator<Item = TransactionPriorityId>,
        num_to_evict: usize,
        mut on_evict: impl FnMut(TransactionPriorityId),
    ) {
        self.queued.extend(ids);
        assert!(
            num_to_evict <= self.queued.len(),
            "num_to_evict must not exceed the number of queued IDs"
        );

        for _ in 0..num_to_evict {
            let id = self.queued.pop_first().expect("queue length checked above");
            on_evict(id);
        }
    }

    /// Returns held transactions to the queue and evicts the requested number of IDs without
    /// reallocating the held buffer.
    pub fn flush_held(&mut self, num_to_evict: usize, on_evict: impl FnMut(TransactionPriorityId)) {
        let mut held = core::mem::take(&mut self.held);
        self.push(held.drain(..), num_to_evict, on_evict);
        self.held = held;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_id_orders_priority_then_id() {
        assert!(TransactionPriorityId::new(1, 2) < TransactionPriorityId::new(2, 1));
        assert!(TransactionPriorityId::new(1, 1) < TransactionPriorityId::new(1, 2));
    }

    #[test]
    fn held_transactions_resume_on_flush() {
        let mut queue = TransactionPriorityQueue::with_capacity(1);
        let id = TransactionPriorityId::new(10, 0);

        queue.hold(id);
        assert!(queue.is_empty());
        queue.flush_held(0, |_| unreachable!());
        assert_eq!(queue.pop_highest(), Some(id));
    }

    #[test]
    fn descending_scan_resumes_below_cursor() {
        let mut queue = TransactionPriorityQueue::with_capacity(4);
        for id in [
            TransactionPriorityId::new(5, 0),
            TransactionPriorityId::new(10, 1),
            TransactionPriorityId::new(5, 2),
            TransactionPriorityId::new(1, 3),
        ] {
            queue.push(std::iter::once(id), 0, |_| unreachable!());
        }

        let mut scan = queue.descending_from(None);
        let first = *scan.next().unwrap();
        let second = *scan.next().unwrap();
        drop(scan);

        assert_eq!(first.priority, 10);
        assert_eq!(second.priority, 5);
        assert_eq!(
            queue
                .descending_from(Some(&second))
                .map(|id| id.priority)
                .collect::<Vec<_>>(),
            [5, 1]
        );
    }

    #[test]
    fn evicts_only_queued_transactions() {
        let mut queue = TransactionPriorityQueue::with_capacity(2);
        let high = TransactionPriorityId::new(10, 0);
        let low = TransactionPriorityId::new(5, 1);
        queue.push([high, low].into_iter(), 0, |_| unreachable!());

        assert_eq!(queue.pop_highest(), Some(high));
        let mut evicted = Vec::new();
        queue.push(std::iter::once(TransactionPriorityId::new(7, 2)), 1, |id| {
            evicted.push(id)
        });
        assert_eq!(evicted, [low]);
    }

    #[test]
    #[should_panic(expected = "num_to_evict must not exceed the number of queued IDs")]
    fn rejects_too_many_evictions() {
        let mut queue = TransactionPriorityQueue::with_capacity(1);
        queue.push(std::iter::empty(), 1, |_| unreachable!());
    }
}
