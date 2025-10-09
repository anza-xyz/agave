//! This module holds [`TransactionBatch`] structure.

use {solana_time_utils::timestamp, tokio_util::bytes::Bytes};

type WiredTransaction = Bytes;

pub trait WorkerPayload:
    IntoIterator<Item = WiredTransaction, IntoIter = std::vec::IntoIter<WiredTransaction>>
    + Clone
    + Send
    + 'static
{
    fn timestamp(&self) -> u64;
}

/// Batch of generated transactions timestamp is used to discard batches which
/// are too old to have valid blockhash.
#[derive(Clone, PartialEq)]
pub struct TransactionBatch {
    wired_transactions: Vec<WiredTransaction>,
    // Time of creation of this batch, used for batch timeouts
    timestamp: u64,
}

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

        Self {
            wired_transactions,
            timestamp: timestamp(),
        }
    }
    pub fn timestamp(&self) -> u64 {
        self.timestamp
    }
}

impl WorkerPayload for TransactionBatch {
    fn timestamp(&self) -> u64 {
        self.timestamp
    }
}
