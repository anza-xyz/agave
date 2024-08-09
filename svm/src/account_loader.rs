use {
    crate::{
        account_overrides::AccountOverrides, account_rent_state::RentState, nonce_info::NonceInfo,
        rollback_accounts::RollbackAccounts, transaction_error_metrics::TransactionErrorMetrics,
        transaction_processing_callback::TransactionProcessingCallback,
    },
    itertools::Itertools,
    solana_compute_budget::compute_budget_limits::ComputeBudgetLimits,
    solana_program_runtime::loaded_programs::{ProgramCacheEntry, ProgramCacheForTxBatch},
    solana_sdk::{
        account::{Account, AccountSharedData, ReadableAccount, WritableAccount},
        feature_set::{self, FeatureSet},
        fee::FeeDetails,
        native_loader,
        nonce::State as NonceState,
        pubkey::Pubkey,
        rent::RentDue,
        rent_collector::{CollectedInfo, RentCollector, RENT_EXEMPT_RENT_EPOCH},
        rent_debits::RentDebits,
        saturating_add_assign,
        sysvar::{
            self,
            instructions::{construct_instructions_data, BorrowedAccountMeta, BorrowedInstruction},
        },
        transaction::{Result, SanitizedTransaction, TransactionError},
        transaction_context::{IndexOfAccount, TransactionAccount},
    },
    solana_svm_transaction::svm_message::SVMMessage,
    solana_system_program::{get_system_account_kind, SystemAccountKind},
    std::{collections::HashMap, num::NonZeroU32},
};

// for the load instructions
pub(crate) type TransactionRent = u64;
pub(crate) type TransactionProgramIndices = Vec<Vec<IndexOfAccount>>;
pub type TransactionProgramIndicestResult = Result<TransactionProgramIndices>;
pub type TransactionLoadAccountResult = Result<Vec<LoadedAccountDetails>>;
pub type TransactionCheckResult = Result<CheckedTransactionDetails>;
pub type TransactionValidationResult = Result<ValidatedTransactionDetails>;
pub type TransactionRentResult = Result<RentDetails>;
pub type TransactionLoadResult = Result<LoadedTransaction>;
pub type UniqueLoadedAccounts = HashMap<Pubkey, AccountSharedData>;

#[derive(Clone)]
#[cfg_attr(feature = "dev-context-only-utils", derive(Default))]
pub struct RentDetails {
    pub rent: TransactionRent,
    pub rent_debits: RentDebits,
    pub loaded_accounts_data_size: u32,
}

#[derive(PartialEq, Eq, Debug, Clone)]
pub struct LoadedAccountDetails {
    pub pubkey: Pubkey,
    pub account_found: bool,
}

#[derive(PartialEq, Eq, Debug, Clone)]
#[cfg_attr(feature = "dev-context-only-utils", derive(Default))]
pub struct CheckedTransactionDetails {
    pub nonce: Option<NonceInfo>,
    pub lamports_per_signature: u64,
}

#[derive(PartialEq, Eq, Debug, Clone)]
#[cfg_attr(feature = "dev-context-only-utils", derive(Default))]
pub struct ValidatedTransactionDetails {
    pub rollback_accounts: RollbackAccounts,
    pub compute_budget_limits: ComputeBudgetLimits,
    pub fee_details: FeeDetails,
    pub fee_payer_account: AccountSharedData,
    pub fee_payer_rent_debit: u64,
}

#[derive(PartialEq, Eq, Debug, Clone)]
#[cfg_attr(feature = "dev-context-only-utils", derive(Default))]
pub struct LoadedTransaction {
    pub accounts: Vec<TransactionAccount>,
    pub program_indices: TransactionProgramIndices,
    pub fee_details: FeeDetails,
    pub rollback_accounts: RollbackAccounts,
    pub compute_budget_limits: ComputeBudgetLimits,
    pub rent: TransactionRent,
    pub rent_debits: RentDebits,
    pub loaded_accounts_data_size: u32,
}

pub fn update_unique_loaded_accounts(
    loaded_transaction: &LoadedTransaction,
    unique_loaded_accounts: &mut UniqueLoadedAccounts,
) {
    loaded_transaction
        .accounts
        .iter()
        .for_each(|(key, account)| {
            unique_loaded_accounts.insert(*key, account.clone());
        });
}

pub fn limited_update_unique_loaded_accounts(
    transaction: &SanitizedTransaction,
    loaded_transaction: &LoadedTransaction,
    unique_loaded_accounts: &mut UniqueLoadedAccounts,
) {
    let message = transaction.message();
    let fee_payer_address = message.fee_payer();
    match &loaded_transaction.rollback_accounts {
        RollbackAccounts::FeePayerOnly { fee_payer_account } => {
            if let Some(account) = unique_loaded_accounts.get_mut(fee_payer_address) {
                *account = fee_payer_account.clone();
            }
        }
        RollbackAccounts::SameNonceAndFeePayer { nonce } => {
            if let Some(account) = unique_loaded_accounts.get_mut(nonce.address()) {
                *account = nonce.account().clone();
            }
        }
        RollbackAccounts::SeparateNonceAndFeePayer {
            nonce,
            fee_payer_account,
        } => {
            if let Some(account) = unique_loaded_accounts.get_mut(fee_payer_address) {
                *account = fee_payer_account.clone();
            }
            if let Some(account) = unique_loaded_accounts.get_mut(nonce.address()) {
                *account = nonce.account().clone();
            }
        }
    }
}

/// Collect rent from an account if rent is still enabled and regardless of
/// whether rent is enabled, set the rent epoch to u64::MAX if the account is
/// rent exempt.
pub fn collect_rent_from_account(
    feature_set: &FeatureSet,
    rent_collector: &RentCollector,
    address: &Pubkey,
    account: &mut AccountSharedData,
) -> CollectedInfo {
    if !feature_set.is_active(&feature_set::disable_rent_fees_collection::id()) {
        rent_collector.collect_from_existing_account(address, account)
    } else {
        // When rent fee collection is disabled, we won't collect rent for any account. If there
        // are any rent paying accounts, their `rent_epoch` won't change either. However, if the
        // account itself is rent-exempted but its `rent_epoch` is not u64::MAX, we will set its
        // `rent_epoch` to u64::MAX. In such case, the behavior stays the same as before.
        if account.rent_epoch() != RENT_EXEMPT_RENT_EPOCH
            && rent_collector.get_rent_due(
                account.lamports(),
                account.data().len(),
                account.rent_epoch(),
            ) == RentDue::Exempt
        {
            account.set_rent_epoch(RENT_EXEMPT_RENT_EPOCH);
        }

        CollectedInfo::default()
    }
}

/// Check whether the payer_account is capable of paying the fee. The
/// side effect is to subtract the fee amount from the payer_account
/// balance of lamports. If the payer_acount is not able to pay the
/// fee, the error_metrics is incremented, and a specific error is
/// returned.
pub fn validate_fee_payer(
    payer_address: &Pubkey,
    payer_account: &mut AccountSharedData,
    payer_index: IndexOfAccount,
    error_metrics: &mut TransactionErrorMetrics,
    rent_collector: &RentCollector,
    fee: u64,
) -> Result<()> {
    if payer_account.lamports() == 0 {
        error_metrics.account_not_found += 1;
        return Err(TransactionError::AccountNotFound);
    }
    let system_account_kind = get_system_account_kind(payer_account).ok_or_else(|| {
        error_metrics.invalid_account_for_fee += 1;
        TransactionError::InvalidAccountForFee
    })?;
    let min_balance = match system_account_kind {
        SystemAccountKind::System => 0,
        SystemAccountKind::Nonce => {
            // Should we ever allow a fees charge to zero a nonce account's
            // balance. The state MUST be set to uninitialized in that case
            rent_collector.rent.minimum_balance(NonceState::size())
        }
    };

    payer_account
        .lamports()
        .checked_sub(min_balance)
        .and_then(|v| v.checked_sub(fee))
        .ok_or_else(|| {
            error_metrics.insufficient_funds += 1;
            TransactionError::InsufficientFundsForFee
        })?;

    let payer_pre_rent_state = RentState::from_account(payer_account, &rent_collector.rent);
    payer_account
        .checked_sub_lamports(fee)
        .map_err(|_| TransactionError::InsufficientFundsForFee)?;

    let payer_post_rent_state = RentState::from_account(payer_account, &rent_collector.rent);
    RentState::check_rent_state_with_account(
        &payer_pre_rent_state,
        &payer_post_rent_state,
        payer_address,
        payer_account,
        payer_index,
    )
}

pub(crate) fn calculate_program_indices<CB: TransactionProcessingCallback>(
    callbacks: &CB,
    txs: &[impl SVMMessage],
    initial_load_results: &mut Vec<TransactionLoadAccountResult>,
    error_metrics: &mut TransactionErrorMetrics,
    unique_loaded_accounts: &mut UniqueLoadedAccounts,
) -> Vec<TransactionProgramIndicestResult> {
    txs.iter()
        .zip(initial_load_results)
        .map(|etx| match etx {
            (message, Ok(loaded_accounts)) => {
                // load transaction indices
                match load_transaction_indices(
                    callbacks,
                    message,
                    loaded_accounts,
                    error_metrics,
                    unique_loaded_accounts,
                ) {
                    Ok(program_indices) => Ok(program_indices),
                    Err(e) => Err(e),
                }
            }
            (_, Err(e)) => Err(e.clone()),
        })
        .collect()
}

fn load_transaction_indices<CB: TransactionProcessingCallback>(
    callbacks: &CB,
    message: &impl SVMMessage,
    accounts: &mut Vec<LoadedAccountDetails>,
    error_metrics: &mut TransactionErrorMetrics,
    unique_loaded_accounts: &mut UniqueLoadedAccounts,
) -> TransactionProgramIndicestResult {
    let builtins_start_index = accounts.len();
    let program_indices = message
        .instructions_iter()
        .map(|instruction| {
            let mut account_indices = Vec::with_capacity(2);
            let program_index = instruction.program_id_index as usize;
            // This command should never return error because the transaction was already added in the unique_loaded_accounts
            let (program_id, program_account) =
                if let Some(loaded_account) = accounts.get(program_index) {
                    let account = unique_loaded_accounts.get(&loaded_account.pubkey).unwrap();
                    (loaded_account.pubkey, account)
                } else {
                    return Err(TransactionError::ProgramAccountNotFound);
                };
            if native_loader::check_id(&program_id) {
                return Ok(account_indices);
            }

            let account_found = accounts
                .get(program_index)
                .map_or(true, |loaded_account| loaded_account.account_found);
            if !account_found {
                error_metrics.account_not_found += 1;
                return Err(TransactionError::ProgramAccountNotFound);
            }

            if !program_account.executable() {
                error_metrics.invalid_program_for_execution += 1;
                return Err(TransactionError::InvalidProgramForExecution);
            }
            account_indices.insert(0, program_index as IndexOfAccount);
            let owner_id = program_account.owner();
            if native_loader::check_id(owner_id) {
                return Ok(account_indices);
            }
            if !accounts
                .get(builtins_start_index..)
                .ok_or(TransactionError::ProgramAccountNotFound)?
                .iter()
                .any(|loaded_account| &loaded_account.pubkey == owner_id)
            {
                if let Some(owner_account) = callbacks.get_account_shared_data(owner_id) {
                    if !native_loader::check_id(owner_account.owner())
                        || !owner_account.executable()
                    {
                        error_metrics.invalid_program_for_execution += 1;
                        return Err(TransactionError::InvalidProgramForExecution);
                    }
                    accounts.push(LoadedAccountDetails {
                        pubkey: *owner_id,
                        account_found: true,
                    });
                    unique_loaded_accounts.insert(*owner_id, owner_account);
                } else {
                    error_metrics.account_not_found += 1;
                    return Err(TransactionError::ProgramAccountNotFound);
                }
            }
            Ok(account_indices)
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(program_indices)
}

// TODO: move where we want to make loaded transactions
/// Collect information about accounts used in txs transactions and
/// return vector of tuples, one for each transaction in the
/// batch. Each tuple contains struct of information about accounts as
/// its first element and an optional transaction nonce info as its
/// second element.
/// Load unique accounts in `unique_loaded_accounts` and returns only loading error
pub(crate) fn load_accounts<CB: TransactionProcessingCallback>(
    callbacks: &CB,
    txs: &[impl SVMMessage],
    check_results: &[TransactionCheckResult],
    account_overrides: Option<&AccountOverrides>,
    loaded_programs: &ProgramCacheForTxBatch,
    unique_loaded_accounts: &mut UniqueLoadedAccounts,
) -> Vec<TransactionLoadAccountResult> {
    txs.iter()
        .zip(check_results.iter())
        .map(|etx| match etx {
            (message, Ok(_)) => {
                // load transactions
                match load_transaction_accounts(
                    callbacks,
                    message,
                    account_overrides,
                    loaded_programs,
                    unique_loaded_accounts,
                ) {
                    Ok(loaded_accounts) => Ok(loaded_accounts),
                    Err(e) => Err(e),
                }
            }
            (_, Err(e)) => Err(e.clone()),
        })
        .collect()
}

fn load_transaction_accounts<CB: TransactionProcessingCallback>(
    callbacks: &CB,
    message: &impl SVMMessage,
    account_overrides: Option<&AccountOverrides>,
    loaded_programs: &ProgramCacheForTxBatch,
    unique_loaded_accounts: &mut UniqueLoadedAccounts,
) -> TransactionLoadAccountResult {
    let account_keys = message.account_keys();

    let instruction_accounts = message
        .instructions_iter()
        .flat_map(|instruction| instruction.accounts)
        .unique()
        .collect::<Vec<&u8>>();

    let accounts = account_keys
        .iter()
        .enumerate() // the key is duplicate dont load again
        .map(|(i, key)| {
            let mut account_found = true;
            #[allow(clippy::collapsible_else_if)]
            let account = if solana_sdk::sysvar::instructions::check_id(key) {
                construct_instructions_account(message)
            } else {
                let instruction_account = u8::try_from(i)
                    .map(|i| instruction_accounts.contains(&&i))
                    .unwrap_or(false);
                if let Some(account_override) =
                    account_overrides.and_then(|overrides| overrides.get(key))
                {
                    account_override.clone()
                } else if let Some(program) = (!instruction_account && !message.is_writable(i))
                    .then_some(())
                    .and_then(|_| loaded_programs.find(key))
                {
                    callbacks
                        .get_account_shared_data(key)
                        .ok_or(TransactionError::AccountNotFound)?;
                    // Optimization to skip loading of accounts which are only used as
                    // programs in top-level instructions and not passed as instruction accounts.
                    account_shared_data_from_program(&program)
                } else {
                    if !unique_loaded_accounts.contains_key(key) {
                        callbacks.get_account_shared_data(key).unwrap_or_else(|| {
                            account_found = false;
                            let mut default_account = AccountSharedData::default();
                            // All new accounts must be rent-exempt (enforced in Bank::execute_loaded_transaction).
                            // Currently, rent collection sets rent_epoch to u64::MAX, but initializing the account
                            // with this field already set would allow us to skip rent collection for these accounts.
                            default_account.set_rent_epoch(RENT_EXEMPT_RENT_EPOCH);
                            default_account
                        })
                    } else {
                        unique_loaded_accounts.get(key).cloned().unwrap_or_else(|| {
                            // this should never happen
                            account_found = false;
                            let mut default_account = AccountSharedData::default();
                            // All new accounts must be rent-exempt (enforced in Bank::execute_loaded_transaction).
                            // Currently, rent collection sets rent_epoch to u64::MAX, but initializing the account
                            // with this field already set would allow us to skip rent collection for these accounts.
                            default_account.set_rent_epoch(RENT_EXEMPT_RENT_EPOCH);
                            default_account
                        })
                    }
                }
            };
            unique_loaded_accounts.insert(*key, account.clone());
            Ok(LoadedAccountDetails {
                pubkey: *key,
                account_found,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(accounts)
}

fn account_shared_data_from_program(loaded_program: &ProgramCacheEntry) -> AccountSharedData {
    // It's an executable program account. The program is already loaded in the cache.
    // So the account data is not needed. Return a dummy AccountSharedData with meta
    // information.
    let mut program_account = AccountSharedData::default();
    program_account.set_owner(loaded_program.account_owner());
    program_account.set_executable(true);
    program_account
}

/// Accumulate loaded account data size into `accumulated_accounts_data_size`.
/// Returns TransactionErr::MaxLoadedAccountsDataSizeExceeded if
/// `accumulated_accounts_data_size` exceeds
/// `requested_loaded_accounts_data_size_limit`.
pub fn accumulate_and_check_loaded_account_data_size(
    accumulated_loaded_accounts_data_size: &mut u32,
    account_data_size: usize,
    requested_loaded_accounts_data_size_limit: NonZeroU32,
    error_metrics: &mut TransactionErrorMetrics,
) -> Result<()> {
    let Ok(account_data_size) = u32::try_from(account_data_size) else {
        error_metrics.max_loaded_accounts_data_size_exceeded += 1;
        return Err(TransactionError::MaxLoadedAccountsDataSizeExceeded);
    };
    saturating_add_assign!(*accumulated_loaded_accounts_data_size, account_data_size);
    if *accumulated_loaded_accounts_data_size > requested_loaded_accounts_data_size_limit.get() {
        error_metrics.max_loaded_accounts_data_size_exceeded += 1;
        Err(TransactionError::MaxLoadedAccountsDataSizeExceeded)
    } else {
        Ok(())
    }
}

fn construct_instructions_account(message: &impl SVMMessage) -> AccountSharedData {
    let account_keys = message.account_keys();
    let mut decompiled_instructions = Vec::with_capacity(message.num_instructions());
    for (program_id, instruction) in message.program_instructions_iter() {
        let accounts = instruction
            .accounts
            .iter()
            .map(|account_index| {
                let account_index = usize::from(*account_index);
                BorrowedAccountMeta {
                    is_signer: message.is_signer(account_index),
                    is_writable: message.is_writable(account_index),
                    pubkey: account_keys.get(account_index).unwrap(),
                }
            })
            .collect();

        decompiled_instructions.push(BorrowedInstruction {
            accounts,
            data: instruction.data,
            program_id,
        });
    }

    AccountSharedData::from(Account {
        data: construct_instructions_data(&decompiled_instructions),
        owner: sysvar::id(),
        ..Account::default()
    })
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            transaction_account_state_info::TransactionAccountStateInfo,
            transaction_processing_callback::TransactionProcessingCallback,
            transaction_processor::TransactionBatchProcessor,
        },
        nonce::state::Versions as NonceVersions,
        solana_compute_budget::{compute_budget::ComputeBudget, compute_budget_limits},
        solana_program_runtime::loaded_programs::{
            BlockRelation, ForkGraph, ProgramCacheEntry, ProgramCacheForTxBatch,
        },
        solana_sdk::{
            account::{Account, AccountSharedData, ReadableAccount, WritableAccount},
            bpf_loader_upgradeable,
            clock::Slot,
            epoch_schedule::EpochSchedule,
            feature_set::FeatureSet,
            hash::Hash,
            instruction::CompiledInstruction,
            message::{
                v0::{LoadedAddresses, LoadedMessage},
                LegacyMessage, Message, MessageHeader, SanitizedMessage,
            },
            native_loader,
            native_token::sol_to_lamports,
            nonce,
            pubkey::Pubkey,
            rent::Rent,
            rent_collector::{RentCollector, RENT_EXEMPT_RENT_EPOCH},
            reserved_account_keys::ReservedAccountKeys,
            signature::{Keypair, Signature, Signer},
            system_program, system_transaction, sysvar,
            transaction::{Result, SanitizedTransaction, Transaction, TransactionError},
            transaction_context::{TransactionAccount, TransactionContext},
        },
        std::{borrow::Cow, collections::HashMap, sync::Arc},
    };

    struct TestForkGraph {}

    impl ForkGraph for TestForkGraph {
        fn relationship(&self, _a: Slot, _b: Slot) -> BlockRelation {
            BlockRelation::Unknown
        }
    }

    #[derive(Default)]
    struct TestCallbacks {
        accounts_map: HashMap<Pubkey, AccountSharedData>,
    }

    impl TransactionProcessingCallback for TestCallbacks {
        fn account_matches_owners(&self, _account: &Pubkey, _owners: &[Pubkey]) -> Option<usize> {
            None
        }

        fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<AccountSharedData> {
            self.accounts_map.get(pubkey).cloned()
        }
    }

    fn load_accounts_with_features_and_rent(
        tx: Transaction,
        accounts: &[TransactionAccount],
        error_metrics: &mut TransactionErrorMetrics,
    ) -> Vec<TransactionLoadResult> {
        let fee_payer_account = accounts[0].1.clone();
        let loaded_programs = ProgramCacheForTxBatch::default();
        let mut accounts_map = HashMap::new();
        for (pubkey, account) in accounts {
            accounts_map.insert(*pubkey, account.clone());
        }
        let callbacks = TestCallbacks { accounts_map };

        let mut unique_loaded_accounts: UniqueLoadedAccounts = HashMap::default();
        let sanitized_tx = SanitizedTransaction::from_transaction_for_tests(tx);
        let load_result = load_transaction_accounts(
            &callbacks,
            sanitized_tx.message(),
            None,
            &loaded_programs,
            &mut unique_loaded_accounts,
        );

        let mut accounts = load_result.clone().unwrap();
        let program_indices = load_transaction_indices(
            &callbacks,
            sanitized_tx.message(),
            &mut accounts,
            error_metrics,
            &mut unique_loaded_accounts,
        );

        let validation_result = ValidatedTransactionDetails {
            fee_payer_account,
            ..ValidatedTransactionDetails::default()
        };

        if load_result.is_err() {
            return vec![Err(load_result.unwrap_err())];
        } else if program_indices.is_err() {
            return vec![Err(program_indices.unwrap_err())];
        }
        let batch_processor = TransactionBatchProcessor::<TestForkGraph>::default();
        let loaded_transaction = batch_processor.create_loaded_transaction(
            &Ok(accounts),
            &program_indices.unwrap(),
            &Ok(validation_result),
            &RentDetails::default(),
            &mut unique_loaded_accounts,
        );

        vec![Ok(loaded_transaction)]
    }

    /// get a feature set with all features activated
    /// with the optional except of 'exclude'
    fn all_features_except(exclude: Option<&[Pubkey]>) -> FeatureSet {
        let mut features = FeatureSet::all_enabled();
        if let Some(exclude) = exclude {
            features.active.retain(|k, _v| !exclude.contains(k));
        }
        features
    }

    fn new_unchecked_sanitized_message(message: Message) -> SanitizedMessage {
        SanitizedMessage::Legacy(LegacyMessage::new(
            message,
            &ReservedAccountKeys::empty_key_set(),
        ))
    }

    fn load_accounts_aux_test(
        tx: Transaction,
        accounts: &[TransactionAccount],
        error_metrics: &mut TransactionErrorMetrics,
    ) -> Vec<TransactionLoadResult> {
        load_accounts_with_features_and_rent(tx, accounts, error_metrics)
    }

    fn load_accounts_with_excluded_features(
        tx: Transaction,
        accounts: &[TransactionAccount],
        error_metrics: &mut TransactionErrorMetrics,
    ) -> Vec<TransactionLoadResult> {
        load_accounts_with_features_and_rent(tx, accounts, error_metrics)
    }

    #[test]
    fn test_load_accounts_unknown_program_id() {
        let mut accounts: Vec<TransactionAccount> = Vec::new();
        let mut error_metrics = TransactionErrorMetrics::default();

        let keypair = Keypair::new();
        let key0 = keypair.pubkey();
        let key1 = Pubkey::from([5u8; 32]);

        let account = AccountSharedData::new(1, 0, &Pubkey::default());
        accounts.push((key0, account));

        let account = AccountSharedData::new(2, 1, &Pubkey::default());
        accounts.push((key1, account));

        let instructions = vec![CompiledInstruction::new(1, &(), vec![0])];
        let tx = Transaction::new_with_compiled_instructions(
            &[&keypair],
            &[],
            Hash::default(),
            vec![Pubkey::default()],
            instructions,
        );

        let loaded_accounts = load_accounts_aux_test(tx, &accounts, &mut error_metrics);

        assert_eq!(error_metrics.account_not_found, 1);
        assert_eq!(loaded_accounts.len(), 1);
        assert_eq!(
            loaded_accounts[0],
            Err(TransactionError::ProgramAccountNotFound)
        );
    }

    #[test]
    fn test_load_accounts_no_loaders() {
        let mut accounts: Vec<TransactionAccount> = Vec::new();
        let mut error_metrics = TransactionErrorMetrics::default();

        let keypair = Keypair::new();
        let key0 = keypair.pubkey();
        let key1 = Pubkey::from([5u8; 32]);

        let mut account = AccountSharedData::new(1, 0, &Pubkey::default());
        account.set_rent_epoch(1);
        accounts.push((key0, account));

        let mut account = AccountSharedData::new(2, 1, &Pubkey::default());
        account.set_rent_epoch(1);
        accounts.push((key1, account));

        let instructions = vec![CompiledInstruction::new(2, &(), vec![0, 1])];
        let tx = Transaction::new_with_compiled_instructions(
            &[&keypair],
            &[key1],
            Hash::default(),
            vec![native_loader::id()],
            instructions,
        );

        let loaded_accounts =
            load_accounts_with_excluded_features(tx, &accounts, &mut error_metrics);

        assert_eq!(error_metrics.account_not_found, 0);
        assert_eq!(loaded_accounts.len(), 1);
        match &loaded_accounts[0] {
            Ok(loaded_transaction) => {
                assert_eq!(loaded_transaction.accounts.len(), 3);
                assert_eq!(loaded_transaction.accounts[0].1, accounts[0].1);
                assert_eq!(loaded_transaction.program_indices.len(), 1);
                assert_eq!(loaded_transaction.program_indices[0].len(), 0);
            }
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn test_load_accounts_bad_owner() {
        let mut accounts: Vec<TransactionAccount> = Vec::new();
        let mut error_metrics = TransactionErrorMetrics::default();

        let keypair = Keypair::new();
        let key0 = keypair.pubkey();
        let key1 = Pubkey::from([5u8; 32]);

        let account = AccountSharedData::new(1, 0, &Pubkey::default());
        accounts.push((key0, account));

        let mut account = AccountSharedData::new(40, 1, &Pubkey::default());
        account.set_owner(bpf_loader_upgradeable::id());
        account.set_executable(true);
        accounts.push((key1, account));

        let instructions = vec![CompiledInstruction::new(1, &(), vec![0])];
        let tx = Transaction::new_with_compiled_instructions(
            &[&keypair],
            &[],
            Hash::default(),
            vec![key1],
            instructions,
        );

        let loaded_accounts = load_accounts_aux_test(tx, &accounts, &mut error_metrics);

        assert_eq!(error_metrics.account_not_found, 1);
        assert_eq!(loaded_accounts.len(), 1);
        assert_eq!(
            loaded_accounts[0],
            Err(TransactionError::ProgramAccountNotFound)
        );
    }

    #[test]
    fn test_load_accounts_not_executable() {
        let mut accounts: Vec<TransactionAccount> = Vec::new();
        let mut error_metrics = TransactionErrorMetrics::default();

        let keypair = Keypair::new();
        let key0 = keypair.pubkey();
        let key1 = Pubkey::from([5u8; 32]);

        let account = AccountSharedData::new(1, 0, &Pubkey::default());
        accounts.push((key0, account));

        let account = AccountSharedData::new(40, 0, &native_loader::id());
        accounts.push((key1, account));

        let instructions = vec![CompiledInstruction::new(1, &(), vec![0])];
        let tx = Transaction::new_with_compiled_instructions(
            &[&keypair],
            &[],
            Hash::default(),
            vec![key1],
            instructions,
        );

        let loaded_accounts = load_accounts_aux_test(tx, &accounts, &mut error_metrics);

        assert_eq!(error_metrics.invalid_program_for_execution, 1);
        assert_eq!(loaded_accounts.len(), 1);
        assert_eq!(
            loaded_accounts[0],
            Err(TransactionError::InvalidProgramForExecution)
        );
    }

    #[test]
    fn test_load_accounts_multiple_loaders() {
        let mut accounts: Vec<TransactionAccount> = Vec::new();
        let mut error_metrics = TransactionErrorMetrics::default();

        let keypair = Keypair::new();
        let key0 = keypair.pubkey();
        let key1 = bpf_loader_upgradeable::id();
        let key2 = Pubkey::from([6u8; 32]);

        let mut account = AccountSharedData::new(1, 0, &Pubkey::default());
        account.set_rent_epoch(1);
        accounts.push((key0, account));

        let mut account = AccountSharedData::new(40, 1, &Pubkey::default());
        account.set_executable(true);
        account.set_rent_epoch(1);
        account.set_owner(native_loader::id());
        accounts.push((key1, account));

        let mut account = AccountSharedData::new(41, 1, &Pubkey::default());
        account.set_executable(true);
        account.set_rent_epoch(1);
        account.set_owner(key1);
        accounts.push((key2, account));

        let instructions = vec![
            CompiledInstruction::new(1, &(), vec![0]),
            CompiledInstruction::new(2, &(), vec![0]),
        ];
        let tx = Transaction::new_with_compiled_instructions(
            &[&keypair],
            &[],
            Hash::default(),
            vec![key1, key2],
            instructions,
        );

        let loaded_accounts =
            load_accounts_with_excluded_features(tx, &accounts, &mut error_metrics);

        assert_eq!(error_metrics.account_not_found, 0);
        assert_eq!(loaded_accounts.len(), 1);
        match &loaded_accounts[0] {
            Ok(loaded_transaction) => {
                assert_eq!(loaded_transaction.accounts.len(), 4);
                assert_eq!(loaded_transaction.accounts[0].1, accounts[0].1);
                assert_eq!(loaded_transaction.program_indices.len(), 2);
                assert_eq!(loaded_transaction.program_indices[0], &[1]);
                assert_eq!(loaded_transaction.program_indices[1], &[2]);
            }
            Err(e) => panic!("{e}"),
        }
    }

    fn load_accounts_no_store(
        accounts: &[TransactionAccount],
        tx: Transaction,
        account_overrides: Option<&AccountOverrides>,
    ) -> Vec<TransactionLoadResult> {
        let sanitized_transaction = SanitizedTransaction::from_transaction_for_tests(tx);

        let mut error_metrics = TransactionErrorMetrics::default();
        let loaded_programs = ProgramCacheForTxBatch::default();
        let mut accounts_map = HashMap::new();
        for (pubkey, account) in accounts {
            accounts_map.insert(*pubkey, account.clone());
        }
        let callbacks = TestCallbacks { accounts_map };

        let mut unique_loaded_accounts: UniqueLoadedAccounts = HashMap::default();
        let load_result = load_transaction_accounts(
            &callbacks,
            sanitized_transaction.message(),
            account_overrides,
            &loaded_programs,
            &mut unique_loaded_accounts,
        );

        let program_indices = load_transaction_indices(
            &callbacks,
            sanitized_transaction.message(),
            &mut load_result.clone().unwrap(),
            &mut error_metrics,
            &mut unique_loaded_accounts,
        );

        let validation_result = ValidatedTransactionDetails::default();

        if load_result.is_err() {
            return vec![Err(load_result.unwrap_err())];
        } else if program_indices.is_err() {
            return vec![Err(program_indices.unwrap_err())];
        }
        let batch_processor = TransactionBatchProcessor::<TestForkGraph>::default();
        let loaded_transaction = batch_processor.create_loaded_transaction(
            &load_result,
            &program_indices.unwrap(),
            &Ok(validation_result),
            &RentDetails::default(),
            &mut unique_loaded_accounts,
        );

        vec![Ok(loaded_transaction)]
    }

    #[test]
    fn test_instructions() {
        solana_logger::setup();
        let instructions_key = solana_sdk::sysvar::instructions::id();
        let keypair = Keypair::new();
        let instructions = vec![CompiledInstruction::new(1, &(), vec![0, 1])];
        let tx = Transaction::new_with_compiled_instructions(
            &[&keypair],
            &[solana_sdk::pubkey::new_rand(), instructions_key],
            Hash::default(),
            vec![native_loader::id()],
            instructions,
        );

        let loaded_accounts = load_accounts_no_store(&[], tx, None);
        assert_eq!(loaded_accounts.len(), 1);
        assert!(loaded_accounts[0].is_err());
    }

    #[test]
    fn test_overrides() {
        solana_logger::setup();
        let mut account_overrides = AccountOverrides::default();
        let slot_history_id = sysvar::slot_history::id();
        let account = AccountSharedData::new(42, 0, &Pubkey::default());
        account_overrides.set_slot_history(Some(account));

        let keypair = Keypair::new();
        let account = AccountSharedData::new(1_000_000, 0, &Pubkey::default());

        let instructions = vec![CompiledInstruction::new(2, &(), vec![0])];
        let tx = Transaction::new_with_compiled_instructions(
            &[&keypair],
            &[slot_history_id],
            Hash::default(),
            vec![native_loader::id()],
            instructions,
        );

        let loaded_accounts =
            load_accounts_no_store(&[(keypair.pubkey(), account)], tx, Some(&account_overrides));
        assert_eq!(loaded_accounts.len(), 1);
        let loaded_transaction = loaded_accounts[0].as_ref().unwrap();
        assert_eq!(loaded_transaction.accounts[0].0, keypair.pubkey());
        assert_eq!(loaded_transaction.accounts[1].0, slot_history_id);
        assert_eq!(loaded_transaction.accounts[1].1.lamports(), 42);
    }

    #[test]
    fn test_accumulate_and_check_loaded_account_data_size() {
        let mut error_metrics = TransactionErrorMetrics::default();
        let mut accumulated_data_size: u32 = 0;
        let data_size: usize = 123;
        let requested_data_size_limit = NonZeroU32::new(data_size as u32).unwrap();

        // OK - loaded data size is up to limit
        assert!(accumulate_and_check_loaded_account_data_size(
            &mut accumulated_data_size,
            data_size,
            requested_data_size_limit,
            &mut error_metrics
        )
        .is_ok());
        assert_eq!(data_size as u32, accumulated_data_size);

        // fail - loading more data that would exceed limit
        let another_byte: usize = 1;
        assert_eq!(
            accumulate_and_check_loaded_account_data_size(
                &mut accumulated_data_size,
                another_byte,
                requested_data_size_limit,
                &mut error_metrics
            ),
            Err(TransactionError::MaxLoadedAccountsDataSizeExceeded)
        );
    }

    struct ValidateFeePayerTestParameter {
        is_nonce: bool,
        payer_init_balance: u64,
        fee: u64,
        expected_result: Result<()>,
        payer_post_balance: u64,
    }
    fn validate_fee_payer_account(
        test_parameter: ValidateFeePayerTestParameter,
        rent_collector: &RentCollector,
    ) {
        let payer_account_keys = Keypair::new();
        let mut account = if test_parameter.is_nonce {
            AccountSharedData::new_data(
                test_parameter.payer_init_balance,
                &NonceVersions::new(NonceState::Initialized(nonce::state::Data::default())),
                &system_program::id(),
            )
            .unwrap()
        } else {
            AccountSharedData::new(test_parameter.payer_init_balance, 0, &system_program::id())
        };
        let result = validate_fee_payer(
            &payer_account_keys.pubkey(),
            &mut account,
            0,
            &mut TransactionErrorMetrics::default(),
            rent_collector,
            test_parameter.fee,
        );

        assert_eq!(result, test_parameter.expected_result);
        assert_eq!(account.lamports(), test_parameter.payer_post_balance);
    }

    #[test]
    fn test_validate_fee_payer() {
        let rent_collector = RentCollector::new(
            0,
            EpochSchedule::default(),
            500_000.0,
            Rent {
                lamports_per_byte_year: 1,
                ..Rent::default()
            },
        );
        let min_balance = rent_collector.rent.minimum_balance(NonceState::size());
        let fee = 5_000;

        // If payer account has sufficient balance, expect successful fee deduction,
        // regardless feature gate status, or if payer is nonce account.
        {
            for (is_nonce, min_balance) in [(true, min_balance), (false, 0)] {
                validate_fee_payer_account(
                    ValidateFeePayerTestParameter {
                        is_nonce,
                        payer_init_balance: min_balance + fee,
                        fee,
                        expected_result: Ok(()),
                        payer_post_balance: min_balance,
                    },
                    &rent_collector,
                );
            }
        }

        // If payer account has no balance, expected AccountNotFound Error
        // regardless feature gate status, or if payer is nonce account.
        {
            for is_nonce in [true, false] {
                validate_fee_payer_account(
                    ValidateFeePayerTestParameter {
                        is_nonce,
                        payer_init_balance: 0,
                        fee,
                        expected_result: Err(TransactionError::AccountNotFound),
                        payer_post_balance: 0,
                    },
                    &rent_collector,
                );
            }
        }

        // If payer account has insufficient balance, expect InsufficientFundsForFee error
        // regardless feature gate status, or if payer is nonce account.
        {
            for (is_nonce, min_balance) in [(true, min_balance), (false, 0)] {
                validate_fee_payer_account(
                    ValidateFeePayerTestParameter {
                        is_nonce,
                        payer_init_balance: min_balance + fee - 1,
                        fee,
                        expected_result: Err(TransactionError::InsufficientFundsForFee),
                        payer_post_balance: min_balance + fee - 1,
                    },
                    &rent_collector,
                );
            }
        }

        // normal payer account has balance of u64::MAX, so does fee; since it does not  require
        // min_balance, expect successful fee deduction, regardless of feature gate status
        {
            validate_fee_payer_account(
                ValidateFeePayerTestParameter {
                    is_nonce: false,
                    payer_init_balance: u64::MAX,
                    fee: u64::MAX,
                    expected_result: Ok(()),
                    payer_post_balance: 0,
                },
                &rent_collector,
            );
        }
    }

    #[test]
    fn test_validate_nonce_fee_payer_with_checked_arithmetic() {
        let rent_collector = RentCollector::new(
            0,
            EpochSchedule::default(),
            500_000.0,
            Rent {
                lamports_per_byte_year: 1,
                ..Rent::default()
            },
        );

        // nonce payer account has balance of u64::MAX, so does fee; due to nonce account
        // requires additional min_balance, expect InsufficientFundsForFee error if feature gate is
        // enabled
        validate_fee_payer_account(
            ValidateFeePayerTestParameter {
                is_nonce: true,
                payer_init_balance: u64::MAX,
                fee: u64::MAX,
                expected_result: Err(TransactionError::InsufficientFundsForFee),
                payer_post_balance: u64::MAX,
            },
            &rent_collector,
        );
    }

    #[test]
    fn test_construct_instructions_account() {
        let loaded_message = LoadedMessage {
            message: Cow::Owned(solana_sdk::message::v0::Message::default()),
            loaded_addresses: Cow::Owned(LoadedAddresses::default()),
            is_writable_account_cache: vec![false],
        };
        let message = SanitizedMessage::V0(loaded_message);
        let shared_data = construct_instructions_account(&message);
        let expected = AccountSharedData::from(Account {
            data: construct_instructions_data(&message.decompile_instructions()),
            owner: sysvar::id(),
            ..Account::default()
        });
        assert_eq!(shared_data, expected);
    }

    #[test]
    fn test_load_transaction_accounts_native_loader() {
        let key1 = Keypair::new();
        let message = Message {
            account_keys: vec![key1.pubkey(), native_loader::id()],
            header: MessageHeader::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 1,
                accounts: vec![0],
                data: vec![],
            }],
            recent_blockhash: Hash::default(),
        };

        let sanitized_message = new_unchecked_sanitized_message(message);
        let mut mock_bank = TestCallbacks::default();
        mock_bank
            .accounts_map
            .insert(native_loader::id(), AccountSharedData::default());
        let mut fee_payer_account_data = AccountSharedData::default();
        fee_payer_account_data.set_lamports(200);
        mock_bank
            .accounts_map
            .insert(key1.pubkey(), fee_payer_account_data.clone());
        let mut unique_loaded_accounts = UniqueLoadedAccounts::default();

        let loaded_programs = ProgramCacheForTxBatch::default();

        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );
        let result = load_transaction_accounts(
            &mock_bank,
            sanitized_transaction.message(),
            None,
            &loaded_programs,
            &mut unique_loaded_accounts,
        );

        let result_keys: Vec<Pubkey> = result
            .unwrap()
            .into_iter()
            .map(|loaded_accounts| loaded_accounts.pubkey)
            .collect();
        let expected_keys = vec![key1.pubkey(), native_loader::id()];
        assert_eq!(result_keys, expected_keys,);

        assert_eq!(
            fee_payer_account_data,
            unique_loaded_accounts.get(&key1.pubkey()).unwrap().clone(),
        );
        assert_eq!(
            mock_bank.accounts_map[&native_loader::id()].clone(),
            unique_loaded_accounts
                .get(&native_loader::id())
                .unwrap()
                .clone(),
        );
    }

    #[test]
    fn test_load_transaction_accounts_program_account_not_found_but_loaded() {
        let key1 = Keypair::new();
        let key2 = Keypair::new();

        let message = Message {
            account_keys: vec![key1.pubkey(), key2.pubkey()],
            header: MessageHeader::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 1,
                accounts: vec![0],
                data: vec![],
            }],
            recent_blockhash: Hash::default(),
        };

        let sanitized_message = new_unchecked_sanitized_message(message);
        let mut mock_bank = TestCallbacks::default();
        let mut account_data = AccountSharedData::default();
        account_data.set_lamports(200);
        mock_bank.accounts_map.insert(key1.pubkey(), account_data);

        let mut loaded_programs = ProgramCacheForTxBatch::default();
        loaded_programs.replenish(key2.pubkey(), Arc::new(ProgramCacheEntry::default()));

        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );
        let mut unique_loaded_accounts: UniqueLoadedAccounts = HashMap::default();
        let result = load_transaction_accounts(
            &mock_bank,
            sanitized_transaction.message(),
            None,
            &loaded_programs,
            &mut unique_loaded_accounts,
        );

        assert_eq!(result.err(), Some(TransactionError::AccountNotFound));
    }

    #[test]
    fn test_load_transaction_accounts_program_account_no_data() {
        let key1 = Keypair::new();
        let key2 = Keypair::new();

        let message = Message {
            account_keys: vec![key1.pubkey(), key2.pubkey()],
            header: MessageHeader::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 1,
                accounts: vec![0, 1],
                data: vec![],
            }],
            recent_blockhash: Hash::default(),
        };

        let sanitized_message = new_unchecked_sanitized_message(message);
        let mut mock_bank = TestCallbacks::default();
        let mut account_data = AccountSharedData::default();
        account_data.set_lamports(200);
        mock_bank
            .accounts_map
            .insert(key1.pubkey(), account_data.clone());

        let mut error_metrics = TransactionErrorMetrics::default();

        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );
        let mut unique_loaded_accounts: UniqueLoadedAccounts = HashMap::default();
        unique_loaded_accounts.insert(key1.pubkey(), account_data);
        let mut accounts = vec![LoadedAccountDetails {
            pubkey: key1.pubkey(),
            account_found: false,
        }];
        let result = load_transaction_indices(
            &mock_bank,
            sanitized_transaction.message(),
            &mut accounts,
            &mut error_metrics,
            &mut unique_loaded_accounts,
        );

        assert_eq!(result.err(), Some(TransactionError::ProgramAccountNotFound));
    }

    #[test]
    fn test_load_transaction_accounts_invalid_program_for_execution() {
        let key1 = Keypair::new();
        let key2 = Keypair::new();

        let message = Message {
            account_keys: vec![key1.pubkey(), key2.pubkey()],
            header: MessageHeader::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 0,
                accounts: vec![0, 1],
                data: vec![],
            }],
            recent_blockhash: Hash::default(),
        };

        let sanitized_message = new_unchecked_sanitized_message(message);
        let mut mock_bank = TestCallbacks::default();
        let mut account_data = AccountSharedData::default();
        account_data.set_lamports(200);
        mock_bank
            .accounts_map
            .insert(key1.pubkey(), account_data.clone());

        let mut error_metrics = TransactionErrorMetrics::default();

        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );

        let loaded_programs = ProgramCacheForTxBatch::default();

        let mut unique_loaded_accounts: UniqueLoadedAccounts = HashMap::default();
        let load_result = load_transaction_accounts(
            &mock_bank,
            sanitized_transaction.message(),
            None,
            &loaded_programs,
            &mut unique_loaded_accounts,
        );

        let result = load_transaction_indices(
            &mock_bank,
            sanitized_transaction.message(),
            &mut load_result.unwrap(),
            &mut error_metrics,
            &mut unique_loaded_accounts,
        );

        assert_eq!(
            result.err(),
            Some(TransactionError::InvalidProgramForExecution)
        );
    }

    #[test]
    fn test_load_transaction_accounts_native_loader_owner() {
        let key1 = Keypair::new();
        let key2 = Keypair::new();

        let message = Message {
            account_keys: vec![key2.pubkey(), key1.pubkey()],
            header: MessageHeader::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 1,
                accounts: vec![0],
                data: vec![],
            }],
            recent_blockhash: Hash::default(),
        };

        let sanitized_message = new_unchecked_sanitized_message(message);
        let mut mock_bank = TestCallbacks::default();
        let mut account_data = AccountSharedData::default();
        account_data.set_owner(native_loader::id());
        account_data.set_executable(true);
        mock_bank
            .accounts_map
            .insert(key1.pubkey(), account_data.clone());

        let mut fee_payer_account_data = AccountSharedData::default();
        fee_payer_account_data.set_lamports(200);
        mock_bank
            .accounts_map
            .insert(key2.pubkey(), fee_payer_account_data.clone());
        let mut error_metrics = TransactionErrorMetrics::default();
        let loaded_programs = ProgramCacheForTxBatch::default();

        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );
        let mut unique_loaded_accounts: UniqueLoadedAccounts = HashMap::default();
        let load_result = load_transaction_accounts(
            &mock_bank,
            sanitized_transaction.message(),
            None,
            &loaded_programs,
            &mut unique_loaded_accounts,
        );

        let program_indices = load_transaction_indices(
            &mock_bank,
            sanitized_transaction.message(),
            &mut load_result.unwrap(),
            &mut error_metrics,
            &mut unique_loaded_accounts,
        );

        assert_eq!(unique_loaded_accounts.len(), 2);
        assert_eq!(
            unique_loaded_accounts
                .get_key_value(&key2.pubkey())
                .unwrap(),
            (&key2.pubkey(), &fee_payer_account_data)
        );
        assert_eq!(
            unique_loaded_accounts
                .get_key_value(&key1.pubkey())
                .unwrap(),
            (
                &key1.pubkey(),
                &mock_bank.accounts_map[&key1.pubkey()].clone()
            )
        );

        assert_eq!(program_indices.unwrap(), vec![vec![1]]);
    }

    #[test]
    fn test_load_transaction_accounts_program_account_not_found_after_all_checks() {
        let key1 = Keypair::new();
        let key2 = Keypair::new();

        let message = Message {
            account_keys: vec![key2.pubkey(), key1.pubkey()],
            header: MessageHeader::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 1,
                accounts: vec![0],
                data: vec![],
            }],
            recent_blockhash: Hash::default(),
        };

        let sanitized_message = new_unchecked_sanitized_message(message);
        let mut mock_bank = TestCallbacks::default();
        let mut account_data = AccountSharedData::default();
        account_data.set_executable(true);
        mock_bank
            .accounts_map
            .insert(key1.pubkey(), account_data.clone());

        let mut account_data = AccountSharedData::default();
        account_data.set_lamports(200);
        mock_bank
            .accounts_map
            .insert(key2.pubkey(), account_data.clone());
        let mut error_metrics = TransactionErrorMetrics::default();
        let loaded_programs = ProgramCacheForTxBatch::default();

        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );
        let mut unique_loaded_accounts: UniqueLoadedAccounts = HashMap::default();
        let load_result = load_transaction_accounts(
            &mock_bank,
            sanitized_transaction.message(),
            None,
            &loaded_programs,
            &mut unique_loaded_accounts,
        );

        let program_indices = load_transaction_indices(
            &mock_bank,
            sanitized_transaction.message(),
            &mut load_result.unwrap(),
            &mut error_metrics,
            &mut unique_loaded_accounts,
        );

        assert_eq!(
            program_indices.err(),
            Some(TransactionError::ProgramAccountNotFound)
        );
    }

    #[test]
    fn test_load_transaction_accounts_program_account_invalid_program_for_execution_last_check() {
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let key3 = Keypair::new();

        let message = Message {
            account_keys: vec![key2.pubkey(), key1.pubkey()],
            header: MessageHeader::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 1,
                accounts: vec![0],
                data: vec![],
            }],
            recent_blockhash: Hash::default(),
        };

        let sanitized_message = new_unchecked_sanitized_message(message);
        let mut mock_bank = TestCallbacks::default();
        let mut account_data = AccountSharedData::default();
        account_data.set_executable(true);
        account_data.set_owner(key3.pubkey());
        mock_bank
            .accounts_map
            .insert(key1.pubkey(), account_data.clone());

        let mut account_data = AccountSharedData::default();
        account_data.set_lamports(200);
        mock_bank
            .accounts_map
            .insert(key2.pubkey(), account_data.clone());

        mock_bank
            .accounts_map
            .insert(key3.pubkey(), AccountSharedData::default());
        let mut error_metrics = TransactionErrorMetrics::default();
        let loaded_programs = ProgramCacheForTxBatch::default();

        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );
        let mut unique_loaded_accounts: UniqueLoadedAccounts = HashMap::default();
        let load_result = load_transaction_accounts(
            &mock_bank,
            sanitized_transaction.message(),
            None,
            &loaded_programs,
            &mut unique_loaded_accounts,
        );

        let program_indices = load_transaction_indices(
            &mock_bank,
            sanitized_transaction.message(),
            &mut load_result.unwrap(),
            &mut error_metrics,
            &mut unique_loaded_accounts,
        );

        assert_eq!(
            program_indices.err(),
            Some(TransactionError::InvalidProgramForExecution)
        );
    }

    #[test]
    fn test_load_transaction_accounts_program_success_complete() {
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let key3 = Keypair::new();

        let message = Message {
            account_keys: vec![key2.pubkey(), key1.pubkey()],
            header: MessageHeader::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 1,
                accounts: vec![0],
                data: vec![],
            }],
            recent_blockhash: Hash::default(),
        };

        let sanitized_message = new_unchecked_sanitized_message(message);
        let mut mock_bank = TestCallbacks::default();
        let mut unique_loaded_accounts: UniqueLoadedAccounts = HashMap::default();
        let mut account_data = AccountSharedData::default();
        account_data.set_executable(true);
        account_data.set_owner(key3.pubkey());
        mock_bank
            .accounts_map
            .insert(key1.pubkey(), account_data.clone());

        let mut fee_payer_account_data = AccountSharedData::default();
        fee_payer_account_data.set_lamports(200);
        mock_bank
            .accounts_map
            .insert(key2.pubkey(), fee_payer_account_data.clone());

        let mut account_data = AccountSharedData::default();
        account_data.set_executable(true);
        account_data.set_owner(native_loader::id());
        mock_bank
            .accounts_map
            .insert(key3.pubkey(), account_data.clone());

        let mut error_metrics = TransactionErrorMetrics::default();
        let loaded_programs = ProgramCacheForTxBatch::default();

        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );

        let load_result = load_transaction_accounts(
            &mock_bank,
            sanitized_transaction.message(),
            None,
            &loaded_programs,
            &mut unique_loaded_accounts,
        );

        let program_indices = load_transaction_indices(
            &mock_bank,
            sanitized_transaction.message(),
            &mut load_result.unwrap(),
            &mut error_metrics,
            &mut unique_loaded_accounts,
        );

        assert_eq!(unique_loaded_accounts.len(), 3);
        assert_eq!(
            unique_loaded_accounts
                .get_key_value(&key2.pubkey())
                .unwrap(),
            (&key2.pubkey(), &fee_payer_account_data)
        );
        assert_eq!(
            unique_loaded_accounts
                .get_key_value(&key1.pubkey())
                .unwrap(),
            (
                &key1.pubkey(),
                &mock_bank.accounts_map[&key1.pubkey()].clone()
            )
        );
        assert_eq!(
            unique_loaded_accounts
                .get_key_value(&key3.pubkey())
                .unwrap(),
            (
                &key3.pubkey(),
                &mock_bank.accounts_map[&key3.pubkey()].clone()
            )
        );

        assert_eq!(program_indices.unwrap(), vec![vec![1]]);
    }

    #[test]
    fn test_load_transaction_accounts_program_builtin_saturating_add() {
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let key3 = Keypair::new();
        let key4 = Keypair::new();

        let message = Message {
            account_keys: vec![key2.pubkey(), key1.pubkey(), key4.pubkey()],
            header: MessageHeader::default(),
            instructions: vec![
                CompiledInstruction {
                    program_id_index: 1,
                    accounts: vec![0],
                    data: vec![],
                },
                CompiledInstruction {
                    program_id_index: 1,
                    accounts: vec![2],
                    data: vec![],
                },
            ],
            recent_blockhash: Hash::default(),
        };

        let sanitized_message = new_unchecked_sanitized_message(message);
        let mut mock_bank = TestCallbacks::default();
        let mut account_data1 = AccountSharedData::default();
        account_data1.set_executable(true);
        account_data1.set_owner(key3.pubkey());
        mock_bank.accounts_map.insert(key1.pubkey(), account_data1);

        let mut fee_payer_account_data = AccountSharedData::default();
        fee_payer_account_data.set_lamports(200);
        mock_bank
            .accounts_map
            .insert(key2.pubkey(), fee_payer_account_data.clone());

        let mut account_data3 = AccountSharedData::default();
        account_data3.set_executable(true);
        account_data3.set_owner(native_loader::id());
        mock_bank
            .accounts_map
            .insert(key3.pubkey(), account_data3.clone());

        let mut error_metrics = TransactionErrorMetrics::default();
        let loaded_programs = ProgramCacheForTxBatch::default();

        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );

        let mut unique_loaded_accounts: UniqueLoadedAccounts = HashMap::default();
        let load_result = load_transaction_accounts(
            &mock_bank,
            sanitized_transaction.message(),
            None,
            &loaded_programs,
            &mut unique_loaded_accounts,
        );

        let program_indices = load_transaction_indices(
            &mock_bank,
            sanitized_transaction.message(),
            &mut load_result.clone().unwrap(),
            &mut error_metrics,
            &mut unique_loaded_accounts,
        );

        assert_eq!(
            unique_loaded_accounts
                .get_key_value(&key1.pubkey())
                .unwrap(),
            (
                &key1.pubkey(),
                &mock_bank.accounts_map[&key1.pubkey()].clone()
            )
        );
        assert_eq!(
            unique_loaded_accounts
                .get_key_value(&key2.pubkey())
                .unwrap(),
            (&key2.pubkey(), &fee_payer_account_data)
        );
        assert_eq!(
            unique_loaded_accounts
                .get_key_value(&key3.pubkey())
                .unwrap(),
            (
                &key3.pubkey(),
                &mock_bank.accounts_map[&key3.pubkey()].clone()
            )
        );

        assert_eq!(program_indices.unwrap(), vec![vec![1], vec![1]]);
    }

    #[test]
    fn test_rent_state_list_len() {
        let mint_keypair = Keypair::new();
        let mut bank = TestCallbacks::default();
        let recipient = Pubkey::new_unique();
        let last_block_hash = Hash::new_unique();

        let mut system_data = AccountSharedData::default();
        system_data.set_executable(true);
        system_data.set_owner(native_loader::id());
        bank.accounts_map
            .insert(Pubkey::new_from_array([0u8; 32]), system_data);

        let mut mint_data = AccountSharedData::default();
        mint_data.set_lamports(2);
        bank.accounts_map.insert(mint_keypair.pubkey(), mint_data);

        bank.accounts_map
            .insert(recipient, AccountSharedData::default());

        let tx = system_transaction::transfer(
            &mint_keypair,
            &recipient,
            sol_to_lamports(1.),
            last_block_hash,
        );
        let num_accounts = tx.message().account_keys.len();
        let sanitized_tx = SanitizedTransaction::from_transaction_for_tests(tx);
        let loaded_programs = ProgramCacheForTxBatch::default();

        let mut unique_loaded_accounts: UniqueLoadedAccounts = HashMap::default();
        let load_result = load_transaction_accounts(
            &bank,
            sanitized_tx.message(),
            None,
            &loaded_programs,
            &mut unique_loaded_accounts,
        );

        let accounts: Vec<(Pubkey, AccountSharedData)> = load_result
            .unwrap()
            .iter()
            .map(|loaded_account| {
                let key = loaded_account.pubkey;
                let account = unique_loaded_accounts.get(&key).unwrap().clone();
                (key, account)
            })
            .collect();

        let compute_budget = ComputeBudget::new(u64::from(
            compute_budget_limits::DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT,
        ));
        let transaction_context = TransactionContext::new(
            accounts,
            Rent::default(),
            compute_budget.max_instruction_stack_depth,
            compute_budget.max_instruction_trace_length,
        );

        assert_eq!(
            TransactionAccountStateInfo::new(
                &Rent::default(),
                &transaction_context,
                sanitized_tx.message()
            )
            .len(),
            num_accounts,
        );
    }

    #[test]
    fn test_load_accounts_and_indices_success() {
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let key3 = Keypair::new();
        let key4 = Keypair::new();

        let message = Message {
            account_keys: vec![key2.pubkey(), key1.pubkey(), key4.pubkey()],
            header: MessageHeader::default(),
            instructions: vec![
                CompiledInstruction {
                    program_id_index: 1,
                    accounts: vec![0],
                    data: vec![],
                },
                CompiledInstruction {
                    program_id_index: 1,
                    accounts: vec![2],
                    data: vec![],
                },
            ],
            recent_blockhash: Hash::default(),
        };

        let sanitized_message = new_unchecked_sanitized_message(message);
        let mut mock_bank = TestCallbacks::default();
        let mut account_data = AccountSharedData::default();
        account_data.set_executable(true);
        account_data.set_owner(key3.pubkey());
        mock_bank.accounts_map.insert(key1.pubkey(), account_data);

        let mut fee_payer_account_data = AccountSharedData::default();
        fee_payer_account_data.set_lamports(200);
        mock_bank
            .accounts_map
            .insert(key2.pubkey(), fee_payer_account_data.clone());

        let mut account_data = AccountSharedData::default();
        account_data.set_executable(true);
        account_data.set_owner(native_loader::id());
        mock_bank
            .accounts_map
            .insert(key3.pubkey(), account_data.clone());

        let mut error_metrics = TransactionErrorMetrics::default();
        let loaded_programs = ProgramCacheForTxBatch::default();

        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );

        let mut unique_loaded_accounts: UniqueLoadedAccounts = HashMap::default();
        let load_result = load_transaction_accounts(
            &mock_bank,
            sanitized_transaction.message(),
            None,
            &loaded_programs,
            &mut unique_loaded_accounts,
        );

        let program_indices = load_transaction_indices(
            &mock_bank,
            sanitized_transaction.message(),
            &mut load_result.clone().unwrap(),
            &mut error_metrics,
            &mut unique_loaded_accounts,
        );

        assert_eq!(
            unique_loaded_accounts
                .get_key_value(&key1.pubkey())
                .unwrap(),
            (
                &key1.pubkey(),
                &mock_bank.accounts_map[&key1.pubkey()].clone()
            )
        );
        assert_eq!(
            unique_loaded_accounts
                .get_key_value(&key2.pubkey())
                .unwrap(),
            (
                &key2.pubkey(),
                &mock_bank.accounts_map[&key2.pubkey()].clone()
            )
        );
        assert_eq!(
            unique_loaded_accounts
                .get_key_value(&key3.pubkey())
                .unwrap(),
            (
                &key3.pubkey(),
                &mock_bank.accounts_map[&key3.pubkey()].clone()
            )
        );

        assert_eq!(program_indices.unwrap(), vec![vec![1], vec![1]]);
    }

    #[test]
    fn test_load_accounts_and_indices_error() {
        let mock_bank = TestCallbacks::default();

        let message = Message {
            account_keys: vec![Pubkey::new_from_array([0; 32])],
            header: MessageHeader::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 0,
                accounts: vec![],
                data: vec![],
            }],
            recent_blockhash: Hash::default(),
        };

        let sanitized_message = new_unchecked_sanitized_message(message);
        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );

        let mut error_metrics = TransactionErrorMetrics::default();
        let loaded_programs = ProgramCacheForTxBatch::default();

        let mut unique_loaded_accounts: UniqueLoadedAccounts = HashMap::default();
        let load_result = load_transaction_accounts(
            &mock_bank,
            sanitized_transaction.message(),
            None,
            &loaded_programs,
            &mut unique_loaded_accounts,
        );

        let program_indices = load_transaction_indices(
            &mock_bank,
            sanitized_transaction.message(),
            &mut load_result.clone().unwrap(),
            &mut error_metrics,
            &mut unique_loaded_accounts,
        );

        assert_eq!(
            program_indices,
            Err(TransactionError::ProgramAccountNotFound)
        );

        let check_results: Vec<TransactionCheckResult> =
            vec![Err(TransactionError::InvalidWritableAccount)];

        let mut unique_loaded_accounts: UniqueLoadedAccounts = HashMap::default();
        let result = load_accounts(
            &mock_bank,
            &vec![sanitized_transaction],
            &check_results,
            None,
            &loaded_programs,
            &mut unique_loaded_accounts,
        );

        assert_eq!(result, vec![Err(TransactionError::InvalidWritableAccount)]);
    }

    #[test]
    fn test_collect_rent_from_account() {
        let feature_set = FeatureSet::all_enabled();
        let rent_collector = RentCollector {
            epoch: 1,
            ..RentCollector::default()
        };

        let address = Pubkey::new_unique();
        let min_exempt_balance = rent_collector.rent.minimum_balance(0);
        let mut account = AccountSharedData::from(Account {
            lamports: min_exempt_balance,
            ..Account::default()
        });

        assert_eq!(
            collect_rent_from_account(&feature_set, &rent_collector, &address, &mut account),
            CollectedInfo::default()
        );
        assert_eq!(account.rent_epoch(), RENT_EXEMPT_RENT_EPOCH);
    }

    #[test]
    fn test_collect_rent_from_account_rent_paying() {
        let feature_set = FeatureSet::all_enabled();
        let rent_collector = RentCollector {
            epoch: 1,
            ..RentCollector::default()
        };

        let address = Pubkey::new_unique();
        let mut account = AccountSharedData::from(Account {
            lamports: 1,
            ..Account::default()
        });

        assert_eq!(
            collect_rent_from_account(&feature_set, &rent_collector, &address, &mut account),
            CollectedInfo::default()
        );
        assert_eq!(account.rent_epoch(), 0);
        assert_eq!(account.lamports(), 1);
    }

    #[test]
    fn test_collect_rent_from_account_rent_enabled() {
        let feature_set =
            all_features_except(Some(&[feature_set::disable_rent_fees_collection::id()]));
        let rent_collector = RentCollector {
            epoch: 1,
            ..RentCollector::default()
        };

        let address = Pubkey::new_unique();
        let mut account = AccountSharedData::from(Account {
            lamports: 1,
            data: vec![0],
            ..Account::default()
        });

        assert_eq!(
            collect_rent_from_account(&feature_set, &rent_collector, &address, &mut account),
            CollectedInfo {
                rent_amount: 1,
                account_data_len_reclaimed: 1
            }
        );
        assert_eq!(account.rent_epoch(), 0);
        assert_eq!(account.lamports(), 0);
    }
}
