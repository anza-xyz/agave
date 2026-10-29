use {
    agave_scheduling_utils::transaction_priority_queue::{
        TransactionPriorityId, TransactionPriorityQueue,
    },
    slab::Slab,
    solana_pubkey::{Pubkey, PubkeyHasherBuilder},
    std::collections::HashMap,
};

struct TransactionEntry<T> {
    priority: u64,
    arrival_order: u64,
    transaction: T,
    nonce_address: Option<Pubkey>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RemovedTransaction<T> {
    NonceReplacement(T),
    CapacityEviction(T),
}

impl<T> RemovedTransaction<T> {
    pub(crate) fn into_transaction(self) -> T {
        match self {
            Self::NonceReplacement(transaction) | Self::CapacityEviction(transaction) => {
                transaction
            }
        }
    }
}

/// Owns transactions until removal, including those popped for scheduling.
///
/// Capacity counts both queued and popped transactions. Only queued transactions may be evicted.
/// IDs are slab indices and may be reused after removal. Earlier arrivals win priority ties,
/// including after requeueing. Callers must retain entries while worker responses are pending.
///
/// `T` owns the transaction and its metadata. Rejection, eviction, and removal return that ownership
/// to the caller; any values still retained when the container is dropped are dropped with it.
pub(crate) struct TransactionContainer<T> {
    capacity: usize,
    transactions: Slab<TransactionEntry<T>>,
    queue: TransactionPriorityQueue,
    next_arrival_order: u64,
    nonces_in_use: HashMap<Pubkey, TransactionPriorityId, PubkeyHasherBuilder>,
}

impl<T> TransactionContainer<T> {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            transactions: Slab::with_capacity(capacity),
            queue: TransactionPriorityQueue::default(),
            next_arrival_order: 0,
            nonces_in_use: HashMap::with_hasher(PubkeyHasherBuilder::default()),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.transactions.len()
    }

    pub(crate) fn should_drop_nonce(&self, address: &Pubkey, priority: u64) -> bool {
        self.nonces_in_use
            .get(address)
            .is_some_and(|existing| existing.priority >= priority || !self.queue.contains(existing))
    }

    /// Inserts and queues a transaction, returning its ID and any evicted transaction.
    ///
    /// At capacity, the incoming priority must exceed the lowest queued priority. Equal priority
    /// arrivals are rejected in favor of earlier transactions. If every transaction is popped,
    /// or capacity is zero, returns the input unchanged.
    pub(crate) fn insert(
        &mut self,
        priority: u64,
        transaction: T,
    ) -> Result<(usize, Option<T>), T> {
        self.insert_with_nonce(priority, transaction, None)
            .map(|(id, removed)| (id, removed.map(RemovedTransaction::into_transaction)))
    }

    /// A validated nonce replaces a lower-priority queued transaction using the same account.
    /// An in-flight or held transaction cannot be replaced.
    /// Returns the inserted ID and at most one removed transaction, with its removal reason.
    pub(crate) fn insert_with_nonce(
        &mut self,
        priority: u64,
        transaction: T,
        nonce_address: Option<Pubkey>,
    ) -> Result<(usize, Option<RemovedTransaction<T>>), T> {
        let nonce_conflict = nonce_address.and_then(|address| {
            self.nonces_in_use
                .get(&address)
                .copied()
                .map(|existing| (address, existing))
        });
        if let Some((_, existing)) = nonce_conflict
            && (existing.priority >= priority || !self.queue.contains(&existing))
        {
            return Err(transaction);
        }
        let removed = if let Some((_, existing)) = nonce_conflict {
            self.remove(existing.id)
                .map(RemovedTransaction::NonceReplacement)
        } else if self.len() == self.capacity {
            if self
                .queue
                .first()
                .is_none_or(|min| priority <= min.priority)
            {
                return Err(transaction);
            }
            let id = self.queue.pop_first().expect("queue has a minimum");
            self.remove(id.id).map(RemovedTransaction::CapacityEviction)
        } else {
            None
        };

        let arrival_order = self.next_arrival_order;
        self.next_arrival_order = self.next_arrival_order.wrapping_add(1);
        let id = self.transactions.insert(TransactionEntry {
            priority,
            arrival_order,
            transaction,
            nonce_address,
        });
        let priority_id = TransactionPriorityId::new(priority, arrival_order, id);
        self.queue.insert(priority_id);
        if let Some(address) = nonce_address {
            self.nonces_in_use.insert(address, priority_id);
        }
        Ok((id, removed))
    }

    pub(crate) fn get(&self, id: usize) -> Option<&T> {
        self.transactions.get(id).map(|entry| &entry.transaction)
    }

    pub(crate) fn get_mut(&mut self, id: usize) -> Option<&mut T> {
        self.transactions
            .get_mut(id)
            .map(|entry| &mut entry.transaction)
    }

    /// Visits queued IDs from highest to lowest priority without removing them.
    pub(crate) fn iter_by_priority(&self) -> impl Iterator<Item = &TransactionPriorityId> {
        self.queue.iter().rev()
    }

    /// Dequeues a transaction while retaining its state and capacity reservation.
    pub(crate) fn dequeue(&mut self, id: &TransactionPriorityId) -> bool {
        self.queue.remove(id)
    }

    /// Pops the highest priority ID while retaining its transaction and capacity reservation.
    pub(crate) fn pop_highest(&mut self) -> Option<usize> {
        self.queue.pop_last().map(|id| id.id)
    }

    /// Requeues a popped transaction with its original priority, arrival order, and ID.
    /// Returns false if the ID is absent or already queued.
    pub(crate) fn requeue(&mut self, id: usize) -> bool {
        let Some(entry) = self.transactions.get(id) else {
            return false;
        };
        self.queue.insert(TransactionPriorityId::new(
            entry.priority,
            entry.arrival_order,
            id,
        ))
    }

    /// Removes a queued or popped transaction and returns ownership to the caller.
    pub(crate) fn remove(&mut self, id: usize) -> Option<T> {
        let entry = self.transactions.try_remove(id)?;
        let priority_id = TransactionPriorityId::new(entry.priority, entry.arrival_order, id);
        self.queue.remove(&priority_id);
        if let Some(address) = entry.nonce_address
            && self.nonces_in_use.get(&address) == Some(&priority_id)
        {
            self.nonces_in_use.remove(&address);
        }
        Some(entry.transaction)
    }
}

#[cfg(test)]
mod tests {
    use {super::*, std::sync::Arc};

    #[test]
    fn popping_retains_transactions_until_removal() {
        let mut container = TransactionContainer::with_capacity(2);
        let (low, _) = container.insert(1, "low").unwrap();
        let (high, _) = container.insert(2, "high").unwrap();
        assert_eq!(container.pop_highest(), Some(high));
        assert_eq!(container.get(high), Some(&"high"));
        *container.get_mut(high).unwrap() = "updated";
        assert_eq!(container.len(), 2);
        assert_eq!(container.remove(high), Some("updated"));
        assert_eq!(container.remove(low), Some("low"));
        assert_eq!(container.pop_highest(), None);
        assert_eq!(container.len(), 0);
        assert_eq!(container.get(high), None);
        assert_eq!(container.get_mut(high), None);
        assert_eq!(container.remove(high), None);
        assert!(!container.requeue(high));
    }

    #[test]
    fn capacity_evicts_lowest_priority_and_preserves_earlier_arrivals() {
        let mut container = TransactionContainer::with_capacity(2);
        let (early, _) = container.insert(1, "early").unwrap();
        let (late, _) = container.insert(1, "late").unwrap();
        assert_eq!(container.insert(0, "lower"), Err("lower"));
        assert_eq!(container.insert(1, "tied"), Err("tied"));
        let (high, evicted) = container.insert(2, "high").unwrap();
        assert_eq!(evicted, Some("late"));
        assert_eq!(high, late);
        assert_eq!(container.get(high), Some(&"high"));
        assert!(!container.requeue(high));
        assert_eq!(container.len(), 2);
        assert_eq!(container.pop_highest(), Some(high));
        assert_eq!(container.pop_highest(), Some(early));
        assert_eq!(container.pop_highest(), None);
    }

    #[test]
    fn popped_transactions_are_never_evicted() {
        let mut container = TransactionContainer::with_capacity(2);
        let (pending, _) = container.insert(1, "pending").unwrap();
        assert_eq!(container.pop_highest(), Some(pending));
        let (queued, _) = container.insert(2, "queued").unwrap();
        let (high, evicted) = container.insert(3, "high").unwrap();
        assert_eq!(evicted, Some("queued"));
        assert_eq!(high, queued);
        assert_eq!(container.get(high), Some(&"high"));
        assert_eq!(container.get(pending), Some(&"pending"));
        assert_eq!(container.pop_highest(), Some(high));
        assert_eq!(container.insert(4, "rejected"), Err("rejected"));
        assert_eq!(container.len(), 2);
        assert!(container.requeue(pending));
        assert!(!container.requeue(pending));
        assert_eq!(container.pop_highest(), Some(pending));
        assert_eq!(container.pop_highest(), None);
    }

    #[test]
    fn removal_reuses_ids_and_requeue_preserves_arrival_order() {
        let mut container = TransactionContainer::with_capacity(2);
        let (removed, _) = container.insert(1, "removed").unwrap();
        let (early, _) = container.insert(1, "early").unwrap();
        assert_eq!(container.remove(removed), Some("removed"));
        let (late, _) = container.insert(1, "late").unwrap();
        assert_eq!(late, removed);
        assert!(late < early);
        assert_eq!(container.pop_highest(), Some(early));
        assert!(container.requeue(early));
        assert_eq!(container.pop_highest(), Some(early));
        assert_eq!(container.pop_highest(), Some(late));
        assert_eq!(container.pop_highest(), None);
    }

    #[test]
    fn eviction_preserves_earlier_arrivals_across_id_reuse() {
        let mut container = TransactionContainer::with_capacity(2);
        let (removed, _) = container.insert(1, "removed").unwrap();
        let (early, _) = container.insert(1, "early").unwrap();
        assert_eq!(container.remove(removed), Some("removed"));
        let (late, _) = container.insert(1, "late").unwrap();
        assert!(late < early);

        let (high, evicted) = container.insert(2, "high").unwrap();
        assert_eq!(evicted, Some("late"));
        assert_eq!(high, late);
        assert_eq!(container.pop_highest(), Some(high));
        assert_eq!(container.pop_highest(), Some(early));
        assert_eq!(container.pop_highest(), None);
    }

    #[test]
    fn zero_capacity_rejects_transactions() {
        let mut zero = TransactionContainer::with_capacity(0);
        assert_eq!(zero.insert(1, "rejected"), Err("rejected"));
        assert_eq!(zero.len(), 0);
        assert_eq!(zero.pop_highest(), None);
    }

    #[test]
    fn nonce_dedup_replaces_only_lower_priority_queued_transactions() {
        let address = Pubkey::new_unique();
        let mut container = TransactionContainer::with_capacity(2);
        let (other, _) = container.insert(0, "other").unwrap();
        let (old, removed) = container
            .insert_with_nonce(1, "old", Some(address))
            .unwrap();
        assert_eq!(removed, None);
        assert!(container.should_drop_nonce(&address, 1));
        assert_eq!(
            container.insert_with_nonce(1, "tie", Some(address)),
            Err("tie")
        );
        let (new, removed) = container
            .insert_with_nonce(2, "new", Some(address))
            .unwrap();
        assert_eq!(removed, Some(RemovedTransaction::NonceReplacement("old")));
        assert_eq!(container.len(), 2);
        assert_eq!(container.get(other), Some(&"other"));
        assert_eq!(container.get(old), Some(&"new"));
        assert_eq!(container.get(new), Some(&"new"));
        assert!(!container.should_drop_nonce(&address, 3));
        assert_eq!(container.pop_highest(), Some(new));
        assert!(container.should_drop_nonce(&address, 3));
        assert_eq!(
            container.insert_with_nonce(3, "late", Some(address)),
            Err("late")
        );
        assert_eq!(container.remove(new), Some("new"));
        assert!(!container.should_drop_nonce(&address, 1));
    }

    #[test]
    fn nonce_index_is_cleared_on_capacity_eviction() {
        let address = Pubkey::new_unique();
        let mut container = TransactionContainer::with_capacity(1);
        container
            .insert_with_nonce(1, "nonce", Some(address))
            .unwrap();
        assert_eq!(
            container.insert_with_nonce(2, "other", None),
            Ok((0, Some(RemovedTransaction::CapacityEviction("nonce"))))
        );
        assert!(!container.should_drop_nonce(&address, 1));
    }

    #[test]
    fn values_are_owned_until_returned_or_container_dropped() {
        let value = Arc::new(());
        let mut container = TransactionContainer::with_capacity(1);
        let (id, _) = container.insert(1, value.clone()).unwrap();
        assert_eq!(Arc::strong_count(&value), 2);
        let rejected = container.insert(1, value.clone()).unwrap_err();
        assert_eq!(Arc::strong_count(&value), 3);
        drop(rejected);
        let (replacement, evicted) = container.insert(2, value.clone()).unwrap();
        assert_eq!(Arc::strong_count(&value), 3);
        assert_eq!(replacement, id);
        drop(evicted);
        assert_eq!(Arc::strong_count(&value), 2);
        container.pop_highest();
        drop(container);
        assert_eq!(Arc::strong_count(&value), 1);
    }
}
