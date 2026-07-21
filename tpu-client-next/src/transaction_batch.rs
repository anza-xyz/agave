//! This module holds [`TransactionBatch`] structure.

use tokio_util::bytes::Bytes;

/// Batch of generated transactions.
#[derive(Clone, PartialEq)]
pub struct TransactionBatch {
    wired_transactions: Vec<WiredTransaction>,
}

type WiredTransaction = Bytes;

impl IntoIterator for TransactionBatch {
    type Item = Bytes;
    type IntoIter = std::vec::IntoIter<Self::Item>;
    fn into_iter(self) -> Self::IntoIter {
        self.wired_transactions.into_iter()
    }
}

impl TransactionBatch {
    pub fn new<T>(wired_transactions: Vec<T>) -> Self
    where
        T: AsRef<[u8]> + Send + 'static,
    {
        let wired_transactions = wired_transactions
            .into_iter()
            .map(|v| Bytes::from_owner(v))
            .collect();

        Self { wired_transactions }
    }
}
