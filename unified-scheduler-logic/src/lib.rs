use solana_runtime_transaction::extended_transaction::ExtendedSanitizedTransaction;

pub struct Task {
    transaction: ExtendedSanitizedTransaction,
    index: usize,
}

impl Task {
    pub fn create_task(transaction: ExtendedSanitizedTransaction, index: usize) -> Self {
        Task { transaction, index }
    }

    pub fn task_index(&self) -> usize {
        self.index
    }

    pub fn transaction(&self) -> &ExtendedSanitizedTransaction {
        &self.transaction
    }
}
