use {
    crate::{
        account_loader::FeesOnlyTransaction,
        transaction_execution_result::{ExecutedTransaction, TransactionExecutionDetails},
    },
    solana_fee_structure::FeeDetails,
    solana_transaction_error::{TransactionError, TransactionResult},
};

pub type TransactionProcessingResult = TransactionResult<ProcessedTransaction>;

pub trait TransactionProcessingResultExtensions {
    fn was_processed(&self) -> bool;
    fn was_processed_with_successful_result(&self) -> bool;
    fn processed_transaction(&self) -> Option<&ProcessedTransaction>;
    fn flattened_result(&self) -> TransactionResult<()>;
}

#[derive(Debug)]
pub enum ProcessedTransaction {
    /// Transaction was executed, but if execution failed, all account state changes
    /// will be rolled back except deducted fees and any advanced nonces
    Executed(Box<ExecutedTransaction>),
    /// Transaction was not able to be executed but fees are able to be
    /// collected and any nonces are advanceable
    FeesOnly(Box<FeesOnlyTransaction>),
}

impl TransactionProcessingResultExtensions for TransactionProcessingResult {
    fn was_processed(&self) -> bool {
        self.is_ok()
    }

    fn was_processed_with_successful_result(&self) -> bool {
        match self {
            Ok(processed_tx) => processed_tx.was_processed_with_successful_result(),
            Err(_) => false,
        }
    }

    fn processed_transaction(&self) -> Option<&ProcessedTransaction> {
        self.as_ref().ok()
    }

    fn flattened_result(&self) -> TransactionResult<()> {
        self.as_ref()
            .map_err(|err| err.clone())
            .and_then(|processed_tx| processed_tx.status())
    }
}

impl ProcessedTransaction {
    fn was_processed_with_successful_result(&self) -> bool {
        match self {
            Self::Executed(executed_tx) => executed_tx.execution_details.status.is_ok(),
            Self::FeesOnly(_) => false,
        }
    }

    pub fn status(&self) -> TransactionResult<()> {
        match self {
            Self::Executed(executed_tx) => executed_tx.execution_details.status.clone(),
            Self::FeesOnly(details) => Err(TransactionError::clone(&details.load_error)),
        }
    }

    pub fn fee_details(&self) -> FeeDetails {
        match self {
            Self::Executed(executed_tx) => executed_tx.loaded_transaction.fee_details,
            Self::FeesOnly(details) => details.fee_details,
        }
    }

    pub fn executed_transaction(&self) -> Option<&ExecutedTransaction> {
        match self {
            Self::Executed(context) => Some(context),
            Self::FeesOnly { .. } => None,
        }
    }

    pub fn execution_details(&self) -> Option<&TransactionExecutionDetails> {
        match self {
            Self::Executed(context) => Some(&context.execution_details),
            Self::FeesOnly { .. } => None,
        }
    }

    pub fn executed_units(&self) -> u64 {
        self.execution_details()
            .map(|detail| detail.executed_units)
            .unwrap_or_default()
    }

    pub fn loaded_accounts_data_size(&self) -> u32 {
        match self {
            Self::Executed(context) => context.loaded_transaction.loaded_accounts_data_size,
            Self::FeesOnly(details) => details.rollback_accounts.data_size() as u32,
        }
    }
}
