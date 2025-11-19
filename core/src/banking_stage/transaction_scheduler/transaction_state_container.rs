#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    super::{transaction_priority_id::TransactionPriorityId, transaction_state::TransactionState},
    crate::banking_stage::scheduler_messages::TransactionId,
    agave_transaction_view::resolved_transaction_view::ResolvedTransactionView,
    itertools::MinMaxResult,
    min_max_heap::MinMaxHeap,
    slab::{Slab, VacantEntry},
    solana_packet::PACKET_DATA_SIZE,
    solana_runtime_transaction::{
        runtime_transaction::RuntimeTransaction, transaction_with_meta::TransactionWithMeta,
    },
    std::sync::Arc,
};

/// This structure will hold `TransactionState` for the entirety of a
/// transaction's lifetime in the scheduler and BankingStage as a whole.
///
/// Transaction Lifetime:
/// 1. Received from `SigVerify` by `BankingStage`
/// 2. Inserted into `TransactionStateContainer` by `BankingStage`
/// 3. Popped in priority-order by scheduler, and transitioned to `Pending` state
/// 4. Processed by `ConsumeWorker`
///    a. If consumed, remove `Pending` state from the `TransactionStateContainer`
///    b. If retryable, transition back to `Unprocessed` state.
///    Re-insert to the queue, and return to step 3.
///
/// The structure is composed of two main components:
/// 1. A priority queue of wrapped `TransactionId`s, which are used to
///    order transactions by priority for selection by the scheduler.
/// 2. A map of `TransactionId` to `TransactionState`, which is used to
///    track the state of each transaction.
///
/// When `Pending`, the associated `TransactionId` is not in the queue, but
/// is still in the map.
/// The entry in the map should exist before insertion into the queue, and be
/// be removed only after the id is removed from the queue.
///
/// The container maintains a fixed capacity. If the queue is full when pushing
/// a new transaction, the lowest priority transaction will be dropped.
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) struct TransactionStateContainer<Tx: TransactionWithMeta> {
    capacity: usize,
    priority_queue: MinMaxHeap<TransactionPriorityId>,
    id_to_transaction_state: Slab<TransactionState<Tx>>,
    held_transactions: Vec<TransactionPriorityId>,
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) trait StateContainer<Tx: TransactionWithMeta> {
    /// Create a new `TransactionStateContainer` with the given capacity.
    fn with_capacity(capacity: usize) -> Self;

    fn queue_size(&self) -> usize;

    fn buffer_size(&self) -> usize;

    /// Returns true if the queue is empty.
    fn is_empty(&self) -> bool;

    /// Get the top transaction id in the priority queue.
    fn pop(&mut self) -> Option<TransactionPriorityId>;

    /// Get mutable transaction state by id.
    fn get_mut_transaction_state(&mut self, id: TransactionId)
        -> Option<&mut TransactionState<Tx>>;

    /// Get reference to `SanitizedTransactionTTL` by id.
    /// Panics if the transaction does not exist.
    fn get_transaction(&self, id: TransactionId) -> Option<&Tx>;

    /// Retries a transaction - inserts transaction back into map.
    /// This transitions the transaction to `Unprocessed` state.
    fn retry_transaction(
        &mut self,
        transaction_id: TransactionId,
        transaction: Tx,
        immediately_retryable: bool,
    ) {
        let transaction_state = self
            .get_mut_transaction_state(transaction_id)
            .expect("transaction must exist");
        let priority_id = TransactionPriorityId::new(transaction_state.priority(), transaction_id);
        transaction_state.retry_transaction(transaction);

        if immediately_retryable {
            self.push_ids_into_queue(std::iter::once(priority_id));
        } else {
            self.hold_transaction(priority_id);
        }
    }

    /// Pushes transaction ids into the priority queue. If the queue if full,
    /// the lowest priority transactions will be dropped (removed from the
    /// queue and map) **after** all ids have been pushed.
    /// To avoid allocating, the caller should not push more than
    /// [`EXTRA_CAPACITY`] ids in a call.
    /// Returns the number of dropped transactions.
    fn push_ids_into_queue(
        &mut self,
        priority_ids: impl Iterator<Item = TransactionPriorityId>,
    ) -> usize;

    /// Hold the tarnsaction until the next flush (next slot).
    fn hold_transaction(&mut self, priority_id: TransactionPriorityId);

    /// Remove transaction by id.
    fn remove_by_id(&mut self, id: TransactionId);

    fn flush_held_transactions(&mut self);

    fn get_min_max_priority(&self) -> MinMaxResult<u64>;

    #[cfg(feature = "dev-context-only-utils")]
    fn clear(&mut self);
}

// Extra capacity is added because some additional space is needed when
// pushing a new transaction into the container to avoid reallocation.
pub(crate) const EXTRA_CAPACITY: usize = 64;

impl<Tx: TransactionWithMeta> StateContainer<Tx> for TransactionStateContainer<Tx> {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            priority_queue: MinMaxHeap::with_capacity(capacity + EXTRA_CAPACITY),
            id_to_transaction_state: Slab::with_capacity(capacity + EXTRA_CAPACITY),
            held_transactions: Vec::with_capacity(capacity),
        }
    }

    fn queue_size(&self) -> usize {
        self.priority_queue.len()
    }

    fn buffer_size(&self) -> usize {
        self.id_to_transaction_state.len()
    }

    fn is_empty(&self) -> bool {
        self.priority_queue.is_empty()
    }

    fn pop(&mut self) -> Option<TransactionPriorityId> {
        self.priority_queue.pop_max()
    }

    fn get_mut_transaction_state(
        &mut self,
        id: TransactionId,
    ) -> Option<&mut TransactionState<Tx>> {
        self.id_to_transaction_state.get_mut(id)
    }

    fn get_transaction(&self, id: TransactionId) -> Option<&Tx> {
        self.id_to_transaction_state
            .get(id)
            .map(|state| state.transaction())
    }

    fn push_ids_into_queue(
        &mut self,
        priority_ids: impl Iterator<Item = TransactionPriorityId>,
    ) -> usize {
        for id in priority_ids {
            self.priority_queue.push(id);
        }

        // The number of items in the `id_to_transaction_state` map is
        // greater than or equal to the number of elements in the queue.
        // To avoid the map going over capacity, we use the length of the
        // map here instead of the queue.
        let num_dropped = self
            .id_to_transaction_state
            .len()
            .saturating_sub(self.capacity);

        for _ in 0..num_dropped {
            let priority_id = self.priority_queue.pop_min().expect("queue is not empty");
            self.id_to_transaction_state.remove(priority_id.id);
        }

        num_dropped
    }

    fn hold_transaction(&mut self, priority_id: TransactionPriorityId) {
        self.held_transactions.push(priority_id);
    }

    fn remove_by_id(&mut self, id: TransactionId) {
        self.id_to_transaction_state.remove(id);
    }

    fn flush_held_transactions(&mut self) {
        let mut held_transactions = core::mem::take(&mut self.held_transactions);
        self.push_ids_into_queue(held_transactions.drain(..));
        core::mem::swap(&mut self.held_transactions, &mut held_transactions);
    }

    fn get_min_max_priority(&self) -> MinMaxResult<u64> {
        match self.priority_queue.peek_min() {
            Some(min) => match self.priority_queue.peek_max() {
                Some(max) => MinMaxResult::MinMax(min.priority, max.priority),
                None => MinMaxResult::OneElement(min.priority),
            },
            None => MinMaxResult::NoElements,
        }
    }

    #[cfg(feature = "dev-context-only-utils")]
    fn clear(&mut self) {
        self.priority_queue.clear();
        self.id_to_transaction_state.clear();
    }
}

impl<Tx: TransactionWithMeta> TransactionStateContainer<Tx> {
    /// Insert a new transaction into the container's queues and maps.
    /// Returns `true` if a packet was dropped due to capacity limits.
    #[cfg(test)]
    pub(crate) fn insert_new_transaction(
        &mut self,
        transaction: Tx,
        max_age: crate::banking_stage::scheduler_messages::MaxAge,
        priority: u64,
        cost: u64,
    ) -> bool {
        let priority_id = {
            let entry = self.get_vacant_map_entry();
            let transaction_id = entry.key();
            entry.insert(TransactionState::new(transaction, max_age, priority, cost));
            TransactionPriorityId::new(priority, transaction_id)
        };

        self.push_ids_into_queue(std::iter::once(priority_id)) > 0
    }

    fn get_vacant_map_entry(&mut self) -> VacantEntry<'_, TransactionState<Tx>> {
        assert!(self.id_to_transaction_state.len() < self.id_to_transaction_state.capacity());
        self.id_to_transaction_state.vacant_entry()
    }
}

pub type SharedBytes = Arc<Vec<u8>>;
pub(crate) type RuntimeTransactionView = RuntimeTransaction<ResolvedTransactionView<SharedBytes>>;
pub(crate) type TransactionViewState = TransactionState<RuntimeTransactionView>;

/// A wrapper around `TransactionStateContainer` that allows reuse of
/// pre-allocated `Bytes` to copy packet data into and use for serialization.
/// This is used to avoid allocations in parsing transactions.
pub struct TransactionViewStateContainer {
    inner: TransactionStateContainer<RuntimeTransactionView>,
    bytes_buffer: Box<[SharedBytes]>,
}

impl TransactionViewStateContainer {
    /// Insert into the map, but NOT into the priority queue.
    /// Returns the id of the transaction if it was inserted.
    pub(crate) fn try_insert_map_only_with_data(
        &mut self,
        data: &[u8],
        f: impl FnOnce(SharedBytes) -> Result<TransactionState<RuntimeTransactionView>, ()>,
    ) -> Option<usize> {
        // Get a vacant entry in the slab.
        let vacant_entry = self.inner.get_vacant_map_entry();
        let transaction_id = vacant_entry.key();

        // Get the vacant space in the bytes buffer.
        let bytes_entry = &mut self.bytes_buffer[transaction_id];
        // Assert the entry is unique, then copy the packet data.
        {
            // The strong count must be 1 here. These are only cloned into the
            // inner container below, wrapped by a `ResolveTransactionView`,
            // which does not expose the backing memory (the `Arc`), or
            // implement `Clone`.
            // This could only fail if there is a bug in the container that the
            // entry in the slab was not cleared. However, since we share
            // indexing between the slab and our `bytes_buffer`, we know that
            // `vacant_entry` is not occupied.
            assert_eq!(Arc::strong_count(bytes_entry), 1, "entry must be unique");
            let bytes = Arc::make_mut(bytes_entry);

            // Clear and copy the packet data into the bytes buffer.
            bytes.clear();
            bytes.extend_from_slice(data);
        }

        // Attempt to insert the transaction.
        if let Ok(state) = f(Arc::clone(bytes_entry)) {
            vacant_entry.insert(state);
            Some(transaction_id)
        } else {
            None
        }
    }
}

impl StateContainer<RuntimeTransactionView> for TransactionViewStateContainer {
    fn with_capacity(capacity: usize) -> Self {
        let inner = TransactionStateContainer::with_capacity(capacity);
        let bytes_buffer = (0..inner.id_to_transaction_state.capacity())
            .map(|_| Arc::new(Vec::with_capacity(PACKET_DATA_SIZE)))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            inner,
            bytes_buffer,
        }
    }

    #[inline]
    fn queue_size(&self) -> usize {
        self.inner.queue_size()
    }

    #[inline]
    fn buffer_size(&self) -> usize {
        self.inner.buffer_size()
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    #[inline]
    fn pop(&mut self) -> Option<TransactionPriorityId> {
        self.inner.pop()
    }

    #[inline]
    fn get_mut_transaction_state(
        &mut self,
        id: TransactionId,
    ) -> Option<&mut TransactionViewState> {
        self.inner.get_mut_transaction_state(id)
    }

    #[inline]
    fn get_transaction(&self, id: TransactionId) -> Option<&RuntimeTransactionView> {
        self.inner.get_transaction(id)
    }

    #[inline]
    fn push_ids_into_queue(
        &mut self,
        priority_ids: impl Iterator<Item = TransactionPriorityId>,
    ) -> usize {
        self.inner.push_ids_into_queue(priority_ids)
    }

    #[inline]
    fn hold_transaction(&mut self, priority_id: TransactionPriorityId) {
        self.inner.hold_transaction(priority_id);
    }

    #[inline]
    fn remove_by_id(&mut self, id: TransactionId) {
        self.inner.remove_by_id(id);
    }

    #[inline]
    fn flush_held_transactions(&mut self) {
        self.inner.flush_held_transactions();
    }

    #[inline]
    fn get_min_max_priority(&self) -> MinMaxResult<u64> {
        self.inner.get_min_max_priority()
    }

    #[cfg(feature = "dev-context-only-utils")]
    #[inline]
    fn clear(&mut self) {
        self.inner.clear();
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::banking_stage::scheduler_messages::MaxAge,
        agave_transaction_view::transaction_view::SanitizedTransactionView,
        solana_compute_budget_interface::ComputeBudgetInstruction,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_message::Message,
        solana_perf::packet::Packet,
        solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
        solana_signer::Signer,
        solana_system_interface::instruction as system_instruction,
        solana_transaction::{
            sanitized::{MessageHash, SanitizedTransaction},
            Transaction,
        },
        std::collections::HashSet,
    };

    /// Returns (transaction_ttl, priority, cost)
    fn test_transaction(
        priority: u64,
    ) -> (RuntimeTransaction<SanitizedTransaction>, MaxAge, u64, u64) {
        let from_keypair = Keypair::new();
        let ixs = vec![
            system_instruction::transfer(&from_keypair.pubkey(), &solana_pubkey::new_rand(), 1),
            ComputeBudgetInstruction::set_compute_unit_price(priority),
        ];
        let message = Message::new(&ixs, Some(&from_keypair.pubkey()));
        let tx = RuntimeTransaction::from_transaction_for_tests(Transaction::new(
            &[&from_keypair],
            message,
            Hash::default(),
        ));
        const TEST_TRANSACTION_COST: u64 = 5000;
        (tx, MaxAge::MAX, priority, TEST_TRANSACTION_COST)
    }

    fn push_to_container(
        container: &mut TransactionStateContainer<RuntimeTransaction<SanitizedTransaction>>,
        num: usize,
    ) {
        for priority in 0..num as u64 {
            let (transaction, max_age, priority, cost) = test_transaction(priority);
            container.insert_new_transaction(transaction, max_age, priority, cost);
        }
    }

    #[test]
    fn test_is_empty() {
        let mut container = TransactionStateContainer::with_capacity(1);
        assert!(container.is_empty());

        push_to_container(&mut container, 1);
        assert!(!container.is_empty());
    }

    #[test]
    fn test_priority_queue_capacity() {
        let mut container = TransactionStateContainer::with_capacity(1);
        push_to_container(&mut container, 5);

        assert_eq!(container.priority_queue.len(), 1);
        assert_eq!(container.id_to_transaction_state.len(), 1);
        assert_eq!(
            container
                .id_to_transaction_state
                .iter()
                .map(|ts| ts.1.priority())
                .next()
                .unwrap(),
            4
        );
    }

    #[test]
    fn test_get_mut_transaction_state() {
        let mut container = TransactionStateContainer::with_capacity(5);
        push_to_container(&mut container, 5);

        let existing_id = 3;
        let non_existing_id = 7;
        assert!(container.get_mut_transaction_state(existing_id).is_some());
        assert!(container.get_mut_transaction_state(existing_id).is_some());
        assert!(container
            .get_mut_transaction_state(non_existing_id)
            .is_none());
    }

    #[test]
    fn test_view_push_ids_to_queue() {
        let mut container = TransactionViewStateContainer::with_capacity(2);

        let reserved_addresses = HashSet::default();
        let packet_parser = |data, priority, cost| {
            let view = SanitizedTransactionView::try_new_sanitized(data, true).unwrap();
            let view = RuntimeTransaction::<SanitizedTransactionView<_>>::try_from(
                view,
                MessageHash::Compute,
                None,
            )
            .unwrap();
            let view = RuntimeTransaction::<ResolvedTransactionView<_>>::try_from(
                view,
                None,
                &reserved_addresses,
            )
            .unwrap();

            Ok(TransactionState::new(view, MaxAge::MAX, priority, cost))
        };

        // Push 2 transactions into the queue so buffer is full.
        for priority in [4, 5] {
            let (transaction, _max_age, priority, cost) = test_transaction(priority);
            let packet = Packet::from_data(None, transaction.to_versioned_transaction()).unwrap();
            let id = container
                .try_insert_map_only_with_data(packet.data(..).unwrap(), |data| {
                    packet_parser(data, priority, cost)
                })
                .unwrap();
            let priority_id = TransactionPriorityId::new(priority, id);
            assert_eq!(
                container.push_ids_into_queue(std::iter::once(priority_id)),
                0
            );
        }

        // Push 5 additional packets in. 5 should be dropped.
        let mut priority_ids = Vec::with_capacity(5);
        for priority in [10, 11, 12, 1, 2] {
            let (transaction, _max_age, priority, cost) = test_transaction(priority);
            let packet = Packet::from_data(None, transaction.to_versioned_transaction()).unwrap();
            let id = container
                .try_insert_map_only_with_data(packet.data(..).unwrap(), |data| {
                    packet_parser(data, priority, cost)
                })
                .unwrap();
            let priority_id = TransactionPriorityId::new(priority, id);
            priority_ids.push(priority_id);
        }
        assert_eq!(container.push_ids_into_queue(priority_ids.into_iter()), 5);
        assert_eq!(container.pop().unwrap().priority, 12);
        assert_eq!(container.pop().unwrap().priority, 11);
        assert!(container.pop().is_none());

        // Container now has no items in the queue, but still has 5 items in the map.
        // If we attempt to push additional transactions to the queue, they
        // are rejected regardless of their priority.
        let priority = u64::MAX;
        let (transaction, _max_age, priority, cost) = test_transaction(priority);
        let packet = Packet::from_data(None, transaction.to_versioned_transaction()).unwrap();
        let id = container
            .try_insert_map_only_with_data(packet.data(..).unwrap(), |data| {
                packet_parser(data, priority, cost)
            })
            .unwrap();
        let priority_id = TransactionPriorityId::new(priority, id);
        assert_eq!(
            container.push_ids_into_queue(std::iter::once(priority_id)),
            1
        );
        assert!(container.pop().is_none());
    }
}
