use {
    crate::account_loader::LoadedTransaction,
    solana_message::inner_instruction::InnerInstructionsList,
    solana_program_runtime::program_cache_entry::ProgramCacheEntry,
    solana_pubkey::Pubkey,
    solana_transaction_context::transaction::TransactionReturnData,
    solana_transaction_error::TransactionResult,
    std::{collections::HashMap, sync::Arc},
};

#[derive(Debug, Default, Clone, PartialEq)]
pub struct TransactionLoadedAccountsStats {
    pub loaded_accounts_data_size: u32,
    pub loaded_accounts_count: usize,
}

#[derive(Debug, Clone)]
pub struct ExecutedTransaction {
    pub loaded_transaction: LoadedTransaction,
    pub execution_details: TransactionExecutionDetails,
    pub programs_modified_by_tx: HashMap<Pubkey, Arc<ProgramCacheEntry>>,
}

impl ExecutedTransaction {
    pub fn was_successful(&self) -> bool {
        self.execution_details.was_successful()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransactionExecutionDetails {
    pub status: TransactionResult<()>,
    pub log_messages: Option<Vec<String>>,
    pub inner_instructions: Option<InnerInstructionsList>,
    pub return_data: Option<TransactionReturnData>,
    pub executed_units: u64,
    /// deltas related to total account data size changes for this transaction.
    /// NOTE: set to None IFF `status` is not `Ok`.
    pub accounts_deltas: Option<AccountsDeltas>,
}

impl TransactionExecutionDetails {
    pub fn was_successful(&self) -> bool {
        self.status.is_ok()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountsDeltas {
    /// aggregate resize delta across all accounts touched by the transaction
    pub accounts_resize_delta: i64,
    /// aggregate size of all accounts that were uninitialized by this transaction
    pub accounts_uninitialized_size: u64,
}
