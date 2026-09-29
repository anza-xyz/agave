use std::{cmp::Ordering, collections::BTreeSet};

/// Queued transaction IDs ordered by priority, then arrival order, then ID.
///
/// Pop the last entry for scheduling or the first entry for eviction. Transaction ownership,
/// capacity enforcement, and assignment of IDs and arrival order belong to the caller.
pub type TransactionPriorityQueue = BTreeSet<TransactionPriorityId>;

/// Orders transactions by priority, favoring earlier arrivals and then smaller IDs on ties.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TransactionPriorityId {
    pub priority: u64,
    pub arrival_order: u64,
    pub id: usize,
}

impl TransactionPriorityId {
    pub fn new(priority: u64, arrival_order: u64, id: usize) -> Self {
        Self {
            priority,
            arrival_order,
            id,
        }
    }
}

impl Ord for TransactionPriorityId {
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority
            .cmp(&other.priority)
            .then_with(|| other.arrival_order.cmp(&self.arrival_order))
            .then_with(|| other.id.cmp(&self.id))
    }
}

impl PartialOrd for TransactionPriorityId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transaction_priority_id_ordering() {
        // Higher priority first
        {
            let id1 = TransactionPriorityId::new(1, 0, 1);
            let id2 = TransactionPriorityId::new(2, 1, 2);
            assert!(id1 < id2);
            assert!(id1 <= id2);
            assert!(id2 > id1);
            assert!(id2 >= id1);
        }

        // Equal priority then compare by arrival order, oldest first
        {
            let id1 = TransactionPriorityId::new(1, 0, 2);
            let id2 = TransactionPriorityId::new(1, 1, 1);
            assert!(id1 > id2);
            assert!(id1 >= id2);
            assert!(id2 < id1);
            assert!(id2 <= id1);
        }

        // Equal priority and arrival order then compare by id, lowest first
        {
            let id1 = TransactionPriorityId::new(1, 0, 1);
            let id2 = TransactionPriorityId::new(1, 0, 2);
            assert!(id1 > id2);
            assert!(id1 >= id2);
            assert!(id2 < id1);
            assert!(id2 <= id1);
        }

        // Equal priority, arrival order, and id
        {
            let id1 = TransactionPriorityId::new(1, 0, 1);
            let id2 = TransactionPriorityId::new(1, 0, 1);
            assert_eq!(id1, id2);
            assert!(id1 >= id2);
            assert!(id1 <= id2);
            assert!(id2 >= id1);
            assert!(id2 <= id1);
        }
    }
}
