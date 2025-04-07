use {
    crate::{
        bank::{Bank, TransactionBalancesSet},
        bank_notification::BankNotification,
    },
    crossbeam_channel::Sender,
    log::*,
    solana_sdk::{clock::Slot, transaction::SanitizedTransaction},
    solana_svm::transaction_commit_result::TransactionCommitResult,
    solana_transaction_status::token_balances::TransactionTokenBalancesSet,
    std::sync::Arc,
};

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum TransactionStatusMessage {
    Batch(TransactionStatusBatch),
    Freeze(Slot),
    /// The message sent to transaction status service so that bank notifications
    /// can be ordered related to the transaction status notifications.
    /// TransactionStatusBatch is first sent, and then the BankNotification such
    /// as Frozen.
    BankEvent(BankNotification),
}

#[derive(Debug)]
pub struct TransactionStatusBatch {
    pub slot: Slot,
    pub transactions: Vec<SanitizedTransaction>,
    pub commit_results: Vec<TransactionCommitResult>,
    pub balances: TransactionBalancesSet,
    pub token_balances: TransactionTokenBalancesSet,
    pub transaction_indexes: Vec<usize>,
}

#[derive(Clone, Debug)]
pub struct TransactionStatusSender {
    pub sender: Sender<TransactionStatusMessage>,
}

impl TransactionStatusSender {
    pub fn send_transaction_status_batch(
        &self,
        slot: Slot,
        transactions: Vec<SanitizedTransaction>,
        commit_results: Vec<TransactionCommitResult>,
        balances: TransactionBalancesSet,
        token_balances: TransactionTokenBalancesSet,
        transaction_indexes: Vec<usize>,
    ) {
        if let Err(e) = self
            .sender
            .send(TransactionStatusMessage::Batch(TransactionStatusBatch {
                slot,
                transactions,
                commit_results,
                balances,
                token_balances,
                transaction_indexes,
            }))
        {
            trace!(
                "Slot {} transaction_status send batch failed: {:?}",
                slot,
                e
            );
        }
    }

    pub fn send_transaction_status_freeze_message(&self, bank: &Arc<Bank>) {
        let slot = bank.slot();
        if let Err(e) = self.sender.send(TransactionStatusMessage::Freeze(slot)) {
            trace!(
                "Slot {} transaction_status send freeze message failed: {:?}",
                slot,
                e
            );
        }
    }
}
