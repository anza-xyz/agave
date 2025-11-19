use {
    agave_scheduler_bindings::worker_message_types::not_included_reasons,
    solana_transaction_error::TransactionError,
};

/// Translate
pub fn transaction_result_to_not_included_reason(result: &Result<(), TransactionError>) -> u8 {
    match result {
        Ok(()) => not_included_reasons::NONE,
        Err(err) => transaction_error_to_not_included_reason(err),
    }
}

pub fn transaction_error_to_not_included_reason(error: &TransactionError) -> u8 {
    match error {
        TransactionError::AccountInUse => not_included_reasons::ACCOUNT_IN_USE,
        TransactionError::AccountLoadedTwice => not_included_reasons::ACCOUNT_LOADED_TWICE,
        TransactionError::AccountNotFound => not_included_reasons::ACCOUNT_NOT_FOUND,
        TransactionError::ProgramAccountNotFound => not_included_reasons::PROGRAM_ACCOUNT_NOT_FOUND,
        TransactionError::InsufficientFundsForFee => {
            not_included_reasons::INSUFFICIENT_FUNDS_FOR_FEE
        }
        TransactionError::InvalidAccountForFee => not_included_reasons::INVALID_ACCOUNT_FOR_FEE,
        TransactionError::AlreadyProcessed => not_included_reasons::ALREADY_PROCESSED,
        TransactionError::BlockhashNotFound => not_included_reasons::BLOCKHASH_NOT_FOUND,
        TransactionError::InstructionError(_, _) => not_included_reasons::INSTRUCTION_ERROR,
        TransactionError::CallChainTooDeep => not_included_reasons::CALL_CHAIN_TOO_DEEP,
        TransactionError::MissingSignatureForFee => not_included_reasons::MISSING_SIGNATURE_FOR_FEE,
        TransactionError::InvalidAccountIndex => not_included_reasons::INVALID_ACCOUNT_INDEX,
        TransactionError::SignatureFailure => not_included_reasons::SIGNATURE_FAILURE,
        TransactionError::InvalidProgramForExecution => {
            not_included_reasons::INVALID_PROGRAM_FOR_EXECUTION
        }
        TransactionError::SanitizeFailure => not_included_reasons::SANITIZE_FAILURE,
        TransactionError::ClusterMaintenance => not_included_reasons::CLUSTER_MAINTENANCE,
        TransactionError::AccountBorrowOutstanding => {
            not_included_reasons::ACCOUNT_BORROW_OUTSTANDING
        }
        TransactionError::WouldExceedMaxBlockCostLimit => {
            not_included_reasons::WOULD_EXCEED_MAX_BLOCK_COST_LIMIT
        }
        TransactionError::UnsupportedVersion => not_included_reasons::UNSUPPORTED_VERSION,
        TransactionError::InvalidWritableAccount => not_included_reasons::INVALID_WRITABLE_ACCOUNT,
        TransactionError::WouldExceedMaxAccountCostLimit => {
            not_included_reasons::WOULD_EXCEED_MAX_ACCOUNT_COST_LIMIT
        }
        TransactionError::WouldExceedAccountDataBlockLimit => {
            not_included_reasons::WOULD_EXCEED_ACCOUNT_DATA_BLOCK_LIMIT
        }
        TransactionError::TooManyAccountLocks => not_included_reasons::TOO_MANY_ACCOUNT_LOCKS,
        TransactionError::AddressLookupTableNotFound => {
            not_included_reasons::ADDRESS_LOOKUP_TABLE_NOT_FOUND
        }
        TransactionError::InvalidAddressLookupTableOwner => {
            not_included_reasons::INVALID_ADDRESS_LOOKUP_TABLE_OWNER
        }
        TransactionError::InvalidAddressLookupTableData => {
            not_included_reasons::INVALID_ADDRESS_LOOKUP_TABLE_DATA
        }
        TransactionError::InvalidAddressLookupTableIndex => {
            not_included_reasons::INVALID_ADDRESS_LOOKUP_TABLE_INDEX
        }
        TransactionError::InvalidRentPayingAccount => {
            not_included_reasons::INVALID_RENT_PAYING_ACCOUNT
        }
        TransactionError::WouldExceedMaxVoteCostLimit => {
            not_included_reasons::WOULD_EXCEED_MAX_VOTE_COST_LIMIT
        }
        TransactionError::WouldExceedAccountDataTotalLimit => {
            not_included_reasons::WOULD_EXCEED_ACCOUNT_DATA_TOTAL_LIMIT
        }
        TransactionError::DuplicateInstruction(_) => not_included_reasons::DUPLICATE_INSTRUCTION,
        TransactionError::InsufficientFundsForRent { .. } => {
            not_included_reasons::INSUFFICIENT_FUNDS_FOR_RENT
        }
        TransactionError::MaxLoadedAccountsDataSizeExceeded => {
            not_included_reasons::MAX_LOADED_ACCOUNTS_DATA_SIZE_EXCEEDED
        }
        TransactionError::InvalidLoadedAccountsDataSizeLimit => {
            not_included_reasons::INVALID_LOADED_ACCOUNTS_DATA_SIZE_LIMIT
        }
        TransactionError::ResanitizationNeeded => not_included_reasons::RESANITIZATION_NEEDED,
        TransactionError::ProgramExecutionTemporarilyRestricted { .. } => {
            not_included_reasons::PROGRAM_EXECUTION_TEMPORARILY_RESTRICTED
        }
        TransactionError::UnbalancedTransaction => not_included_reasons::UNBALANCED_TRANSACTION,
        TransactionError::ProgramCacheHitMaxLimit => {
            not_included_reasons::PROGRAM_CACHE_HIT_MAX_LIMIT
        }

        // SPECIAL CASE - CommitCancelled is an internal error reused to avoid breaking sdk
        TransactionError::CommitCancelled => not_included_reasons::ALL_OR_NOTHING_BATCH_FAILURE,
    }
}
