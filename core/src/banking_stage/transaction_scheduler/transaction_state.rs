use crate::banking_stage::scheduler_messages::MaxAge;
#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;

/// TransactionState is used to track the state of a transaction in the transaction scheduler
/// and banking stage as a whole.
///
/// Newly received transactions initially have `Some(transaction)`.
/// When a transaction is scheduled, the transaction is taken from the Option.
/// When a transaction finishes processing it may be retryable. If it is
/// retryable, the transaction is added back into the Option. If it si not
/// retryable, the state is dropped.
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) struct TransactionState<Tx> {
    /// If `Some`, the transaction is available for scheduling.
    /// If `None`, the transaction is currently scheduled or being processed.
    transaction: Option<Tx>,
    /// Tracks information on the maximum age the transaction's pre-processing
    /// is valid for. This includes sanitization features, as well as resolved
    /// address lookups.
    max_age: MaxAge,
    /// Priority of the transaction.
    priority: u64,
    /// Estimated cost of the transaction.
    cost: u64,
}

impl<Tx> TransactionState<Tx> {
    /// Creates a new `TransactionState` in the `Unprocessed` state.
    pub(crate) fn new(transaction: Tx, max_age: MaxAge, priority: u64, cost: u64) -> Self {
        Self {
            transaction: Some(transaction),
            max_age,
            priority,
            cost,
        }
    }

    /// Return the priority of the transaction.
    /// This is *not* the same as the `compute_unit_price` of the transaction.
    /// The priority is used to order transactions for processing.
    pub(crate) fn priority(&self) -> u64 {
        self.priority
    }

    /// Return the cost of the transaction.
    pub(crate) fn cost(&self) -> u64 {
        self.cost
    }

    /// Intended to be called when a transaction is scheduled. This method
    /// takes ownership of the transaction from the state.
    ///
    /// # Panics
    /// This method will panic if the transaction has already been scheduled.
    pub(crate) fn take_transaction_for_scheduling(&mut self) -> (Tx, MaxAge) {
        let tx = self
            .transaction
            .take()
            .expect("transaction not already pending");
        (tx, self.max_age)
    }

    /// Intended to be called when a transaction is retried. This method will
    /// return ownership of the transaction to the state.
    ///
    /// # Panics
    /// This method will panic if the transaction is not pending.
    pub(crate) fn retry_transaction(&mut self, transaction: Tx) {
        assert!(
            self.transaction.replace(transaction).is_none(),
            "transaction is pending"
        );
    }

    /// Get a reference to the transaction.
    ///
    /// # Panics
    /// This method will panic if the transaction is in the `Pending` state.
    pub(crate) fn transaction(&self) -> &Tx {
        self.transaction
            .as_ref()
            .expect("transaction is not pending")
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_compute_budget_interface::ComputeBudgetInstruction,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_message::Message,
        solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
        solana_signer::Signer,
        solana_system_interface::instruction as system_instruction,
        solana_transaction::{sanitized::SanitizedTransaction, Transaction},
    };

    fn create_transaction_state(
        compute_unit_price: u64,
    ) -> TransactionState<RuntimeTransaction<SanitizedTransaction>> {
        let from_keypair = Keypair::new();
        let ixs = vec![
            system_instruction::transfer(&from_keypair.pubkey(), &solana_pubkey::new_rand(), 1),
            ComputeBudgetInstruction::set_compute_unit_price(compute_unit_price),
        ];
        let message = Message::new(&ixs, Some(&from_keypair.pubkey()));
        let tx = Transaction::new(&[&from_keypair], message, Hash::default());

        const TEST_TRANSACTION_COST: u64 = 5000;
        TransactionState::new(
            RuntimeTransaction::from_transaction_for_tests(tx),
            MaxAge::MAX,
            compute_unit_price,
            TEST_TRANSACTION_COST,
        )
    }

    #[test]
    #[should_panic(expected = "already pending")]
    fn test_take_transaction_for_scheduling_panic() {
        let mut transaction_state = create_transaction_state(0);
        transaction_state.take_transaction_for_scheduling();
        transaction_state.take_transaction_for_scheduling(); // invalid transition
    }

    #[test]
    fn test_take_transaction_for_scheduling() {
        let mut transaction_state = create_transaction_state(0);
        assert!(transaction_state.transaction.is_some());
        let _ = transaction_state.take_transaction_for_scheduling();
        assert!(transaction_state.transaction.is_none());
    }

    #[test]
    #[should_panic(expected = "transaction is pending")]
    fn test_retry_transaction_panic() {
        let mut transaction_state = create_transaction_state(0);
        // invalid transition since transaction is already some
        assert!(transaction_state.transaction.is_some());
        let transaction_clone = transaction_state.transaction.as_ref().unwrap().clone();
        assert!(transaction_state.transaction.is_some());
        transaction_state.retry_transaction(transaction_clone);
    }

    #[test]
    fn test_retry_transaction() {
        let mut transaction_state = create_transaction_state(0);
        assert!(transaction_state.transaction.is_some());
        let (transaction, _max_age) = transaction_state.take_transaction_for_scheduling();
        assert!(transaction_state.transaction.is_none());
        transaction_state.retry_transaction(transaction);
        assert!(transaction_state.transaction.is_some());
    }

    #[test]
    fn test_priority() {
        let priority = 15;
        let mut transaction_state = create_transaction_state(priority);
        assert_eq!(transaction_state.priority(), priority);

        // ensure compute unit price is not lost through state transitions
        let (transaction, _max_age) = transaction_state.take_transaction_for_scheduling();
        assert_eq!(transaction_state.priority(), priority);
        transaction_state.retry_transaction(transaction);
        assert_eq!(transaction_state.priority(), priority);
    }

    #[test]
    #[should_panic(expected = "transaction is not pending")]
    fn test_transaction_panic() {
        let mut transaction_state = create_transaction_state(0);
        let _transaction = transaction_state.transaction();
        assert!(transaction_state.transaction.is_some());

        let _ = transaction_state.take_transaction_for_scheduling();
        assert!(transaction_state.transaction.is_none());
        let _ = transaction_state.transaction(); // pending state, the transaction ttl is not available
    }

    #[test]
    fn test_transaction() {
        let mut transaction_state = create_transaction_state(0);
        let _transaction = transaction_state.transaction();
        assert!(transaction_state.transaction.is_some());

        // ensure transaction_ttl is not lost through state transitions
        let (transaction, _max_age) = transaction_state.take_transaction_for_scheduling();
        assert!(transaction_state.transaction.is_none());

        transaction_state.retry_transaction(transaction);
        let _transaction = transaction_state.transaction();
        assert!(transaction_state.transaction.is_some());
    }
}
