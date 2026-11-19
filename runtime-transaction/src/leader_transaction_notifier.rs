//! Bounded, nonblocking delivery of local leader execution observations.
use {
    crate::transaction_with_meta::StaticTransactionWithMeta,
    crossbeam_channel::{Receiver, Sender, bounded},
    solana_clock::{BankId, Slot},
    solana_transaction_error::TransactionResult,
    std::{
        ops::Range,
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
    },
};

/// One copy of transaction wire bytes per execution batch. Construct before taking
/// the bank freeze lock, then enqueue only after recording succeeds.
#[derive(Debug)]
pub struct LeaderTransactionBatch {
    pub slot: Slot,
    pub bank_id: BankId,
    pub starting_index: usize,
    bytes: Vec<u8>,
    transactions: Vec<(Range<usize>, TransactionResult<()>)>,
}

impl LeaderTransactionBatch {
    pub fn new(slot: Slot, bank_id: BankId, count: usize, bytes: usize) -> Self {
        Self {
            slot,
            bank_id,
            starting_index: 0,
            bytes: Vec::with_capacity(bytes),
            transactions: Vec::with_capacity(count),
        }
    }

    pub fn push(
        &mut self,
        transaction: &impl StaticTransactionWithMeta,
        status: TransactionResult<()>,
    ) {
        let start = self.bytes.len();
        transaction.append_transaction_bytes(&mut self.bytes);
        self.transactions.push((start..self.bytes.len(), status));
    }

    pub fn len(&self) -> usize {
        self.transactions.len()
    }
    pub fn is_empty(&self) -> bool {
        self.transactions.is_empty()
    }

    pub fn transactions(&self) -> impl ExactSizeIterator<Item = (&[u8], &TransactionResult<()>)> {
        self.transactions
            .iter()
            .map(|(range, status)| (&self.bytes[range.clone()], status))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaderBankStatus {
    Created,
    Frozen { transaction_count: u64 },
    Abandoned { transaction_count: u64 },
}

#[derive(Debug)]
pub enum LeaderNotification {
    Transactions(LeaderTransactionBatch),
    Bank {
        slot: Slot,
        parent: Slot,
        bank_id: BankId,
        status: LeaderBankStatus,
    },
}

#[derive(Debug, Default)]
struct Dropped {
    transactions: AtomicU64,
    bank_statuses: AtomicU64,
}

#[derive(Clone, Debug)]
pub struct LeaderTransactionNotifier {
    sender: Sender<LeaderNotification>,
    dropped: Arc<Dropped>,
}

impl LeaderTransactionNotifier {
    /// Capacity is measured in batches/lifecycle events. A full queue never blocks
    /// banking and drops the entire submitted batch, counting every transaction.
    pub fn channel(capacity: usize) -> (Self, Receiver<LeaderNotification>) {
        let (sender, receiver) = bounded(capacity);
        (
            Self {
                sender,
                dropped: Arc::default(),
            },
            receiver,
        )
    }

    pub fn dropped(&self) -> (u64, u64) {
        (
            self.dropped.transactions.load(Ordering::Relaxed),
            self.dropped.bank_statuses.load(Ordering::Relaxed),
        )
    }

    pub fn notify_transactions(&self, mut batch: LeaderTransactionBatch, starting_index: usize) {
        if batch.is_empty() {
            return;
        }
        batch.starting_index = starting_index;
        let count = batch.len() as u64;
        if self
            .sender
            .try_send(LeaderNotification::Transactions(batch))
            .is_err()
        {
            self.dropped
                .transactions
                .fetch_add(count, Ordering::Relaxed);
        }
    }

    pub fn notify_bank(&self, slot: Slot, parent: Slot, bank_id: BankId, status: LeaderBankStatus) {
        if self
            .sender
            .try_send(LeaderNotification::Bank {
                slot,
                parent,
                bank_id,
                status,
            })
            .is_err()
        {
            self.dropped.bank_statuses.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(count: usize) -> LeaderTransactionBatch {
        LeaderTransactionBatch {
            slot: 42,
            bank_id: 7,
            starting_index: 0,
            bytes: vec![1; count],
            transactions: (0..count).map(|i| (i..i + 1, Ok(()))).collect(),
        }
    }

    #[test]
    fn full_queue_counts_whole_batch_and_lifecycle_loss() {
        let (notifier, receiver) = LeaderTransactionNotifier::channel(1);
        notifier.notify_bank(42, 41, 7, LeaderBankStatus::Created);
        notifier.notify_transactions(batch(3), 0);
        notifier.notify_bank(
            42,
            41,
            7,
            LeaderBankStatus::Frozen {
                transaction_count: 3,
            },
        );
        assert_eq!(notifier.dropped(), (3, 1));
        assert!(matches!(
            receiver.recv().unwrap(),
            LeaderNotification::Bank {
                bank_id: 7,
                status: LeaderBankStatus::Created,
                ..
            }
        ));
        notifier.notify_transactions(batch(2), 3);
        let LeaderNotification::Transactions(batch) = receiver.recv().unwrap() else {
            panic!()
        };
        assert_eq!(batch.starting_index, 3);
        assert_eq!(batch.transactions().count(), 2);
        assert_eq!(notifier.dropped(), (3, 1));
    }

    #[test]
    fn disconnected_queue_and_empty_batches() {
        let (notifier, receiver) = LeaderTransactionNotifier::channel(1);
        drop(receiver);
        notifier.notify_transactions(batch(0), 0);
        notifier.notify_transactions(batch(4), 0);
        notifier.notify_bank(
            42,
            41,
            7,
            LeaderBankStatus::Abandoned {
                transaction_count: 4,
            },
        );
        assert_eq!(notifier.dropped(), (4, 1));
    }

    #[test]
    fn replacement_bank_has_distinct_lifecycle() {
        let (notifier, receiver) = LeaderTransactionNotifier::channel(4);
        notifier.notify_bank(42, 41, 7, LeaderBankStatus::Created);
        notifier.notify_bank(
            42,
            41,
            7,
            LeaderBankStatus::Abandoned {
                transaction_count: 0,
            },
        );
        notifier.notify_bank(42, 40, 8, LeaderBankStatus::Created);
        let events: Vec<_> = receiver.try_iter().collect();
        assert!(matches!(
            events[0],
            LeaderNotification::Bank {
                bank_id: 7,
                status: LeaderBankStatus::Created,
                ..
            }
        ));
        assert!(matches!(
            events[1],
            LeaderNotification::Bank {
                bank_id: 7,
                status: LeaderBankStatus::Abandoned { .. },
                ..
            }
        ));
        assert!(matches!(
            events[2],
            LeaderNotification::Bank {
                bank_id: 8,
                parent: 40,
                status: LeaderBankStatus::Created,
                ..
            }
        ));
    }
}
