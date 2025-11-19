#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::{field_qualifiers, qualifiers};
use {
    crate::{
        account_overrides::AccountOverrides,
        nonce_info::NonceInfo,
        rent_calculator::{
            check_rent_state_with_account, get_account_rent_state, RENT_EXEMPT_RENT_EPOCH,
        },
        rollback_accounts::RollbackAccounts,
        transaction_error_metrics::TransactionErrorMetrics,
    },
    ahash::{AHashMap, AHashSet},
    solana_account::{
        state_traits::StateMut, Account, AccountSharedData, ReadableAccount, WritableAccount,
        PROGRAM_OWNERS,
    },
    solana_clock::Slot,
    solana_fee_structure::FeeDetails,
    solana_instruction::{BorrowedAccountMeta, BorrowedInstruction},
    solana_instructions_sysvar::construct_instructions_data,
    solana_loader_v3_interface::state::UpgradeableLoaderState,
    solana_nonce::state::State as NonceState,
    solana_nonce_account::{get_system_account_kind, SystemAccountKind},
    solana_program_runtime::execution_budget::{
        SVMTransactionExecutionAndFeeBudgetLimits, SVMTransactionExecutionBudget,
    },
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_sdk_ids::{
        bpf_loader_upgradeable, native_loader,
        sysvar::{self, slot_history},
    },
    solana_svm_callback::{AccountState, TransactionProcessingCallback},
    solana_svm_feature_set::SVMFeatureSet,
    solana_svm_transaction::svm_message::SVMMessage,
    solana_transaction_context::{transaction_accounts::KeyedAccountSharedData, IndexOfAccount},
    solana_transaction_error::{TransactionError, TransactionResult as Result},
    std::num::{NonZeroU32, Saturating},
};

// Per SIMD-0186, all accounts are assigned a base size of 64 bytes to cover
// the storage cost of metadata.
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) const TRANSACTION_ACCOUNT_BASE_SIZE: usize = 64;

// Per SIMD-0186, resolved address lookup tables are assigned a base size of 8248
// bytes: 8192 bytes for the maximum table size plus 56 bytes for metadata.
const ADDRESS_LOOKUP_TABLE_BASE_SIZE: usize = 8248;

// for the load instructions
pub type TransactionCheckResult = Result<CheckedTransactionDetails>;
type TransactionValidationResult = Result<ValidatedTransactionDetails>;

#[derive(PartialEq, Eq, Debug)]
pub(crate) enum TransactionLoadResult {
    /// All transaction accounts were loaded successfully
    Loaded(LoadedTransaction),
    /// Some transaction accounts needed for execution were unable to be loaded
    /// but the fee payer and any nonce account needed for fee collection were
    /// loaded successfully
    FeesOnly(FeesOnlyTransaction),
    /// Some transaction accounts needed for fee collection were unable to be
    /// loaded
    NotLoaded(TransactionError),
}

#[derive(PartialEq, Eq, Debug, Clone)]
#[cfg_attr(feature = "svm-internal", field_qualifiers(nonce(pub)))]
pub struct CheckedTransactionDetails {
    pub(crate) nonce: Option<NonceInfo>,
    pub(crate) compute_budget_and_limits: Result<SVMTransactionExecutionAndFeeBudgetLimits>,
}

#[cfg(feature = "dev-context-only-utils")]
impl Default for CheckedTransactionDetails {
    fn default() -> Self {
        Self {
            nonce: None,
            compute_budget_and_limits: Ok(SVMTransactionExecutionAndFeeBudgetLimits {
                budget: SVMTransactionExecutionBudget::default(),
                loaded_accounts_data_size_limit: NonZeroU32::new(32)
                    .expect("Failed to set loaded_accounts_bytes"),
                fee_details: FeeDetails::default(),
            }),
        }
    }
}

impl CheckedTransactionDetails {
    pub fn new(
        nonce: Option<NonceInfo>,
        compute_budget_and_limits: Result<SVMTransactionExecutionAndFeeBudgetLimits>,
    ) -> Self {
        Self {
            nonce,
            compute_budget_and_limits,
        }
    }
}

#[derive(PartialEq, Eq, Debug, Clone)]
pub(crate) struct ValidatedTransactionDetails {
    pub(crate) rollback_accounts: RollbackAccounts,
    pub(crate) compute_budget: SVMTransactionExecutionBudget,
    pub(crate) loaded_accounts_bytes_limit: NonZeroU32,
    pub(crate) fee_details: FeeDetails,
    pub(crate) loaded_fee_payer_account: LoadedTransactionAccount,
}

#[cfg(feature = "dev-context-only-utils")]
impl Default for ValidatedTransactionDetails {
    fn default() -> Self {
        Self {
            rollback_accounts: RollbackAccounts::default(),
            compute_budget: SVMTransactionExecutionBudget::default(),
            loaded_accounts_bytes_limit:
                solana_program_runtime::execution_budget::MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
            fee_details: FeeDetails::default(),
            loaded_fee_payer_account: LoadedTransactionAccount::default(),
        }
    }
}

#[derive(PartialEq, Eq, Debug, Clone)]
#[cfg_attr(feature = "dev-context-only-utils", derive(Default))]
pub(crate) struct LoadedTransactionAccount {
    pub(crate) account: AccountSharedData,
    pub(crate) loaded_size: usize,
}

#[derive(PartialEq, Eq, Debug, Clone)]
#[cfg_attr(feature = "dev-context-only-utils", derive(Default))]
#[cfg_attr(
    feature = "dev-context-only-utils",
    field_qualifiers(program_indices(pub), compute_budget(pub))
)]
pub struct LoadedTransaction {
    pub accounts: Vec<KeyedAccountSharedData>,
    pub(crate) program_indices: Vec<IndexOfAccount>,
    pub fee_details: FeeDetails,
    pub rollback_accounts: RollbackAccounts,
    pub(crate) compute_budget: SVMTransactionExecutionBudget,
    pub loaded_accounts_data_size: u32,
}

#[derive(PartialEq, Eq, Debug, Clone)]
pub struct FeesOnlyTransaction {
    pub load_error: TransactionError,
    pub rollback_accounts: RollbackAccounts,
    pub fee_details: FeeDetails,
}

// This is an internal SVM type that tracks account changes throughout a
// transaction batch and obviates the need to load accounts from accounts-db
// more than once. It effectively wraps an `impl TransactionProcessingCallback`
// type, and itself implements `TransactionProcessingCallback`, behaving
// exactly like the implementor of the trait, but also returning up-to-date
// account states mid-batch.
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) struct AccountLoader<'a, CB: TransactionProcessingCallback> {
    loaded_accounts: AHashMap<Pubkey, AccountSharedData>,
    callbacks: &'a CB,
    pub(crate) feature_set: &'a SVMFeatureSet,
}

impl<'a, CB: TransactionProcessingCallback> AccountLoader<'a, CB> {
    // create a new AccountLoader for the transaction batch
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn new_with_loaded_accounts_capacity(
        account_overrides: Option<&'a AccountOverrides>,
        callbacks: &'a CB,
        feature_set: &'a SVMFeatureSet,
        capacity: usize,
    ) -> AccountLoader<'a, CB> {
        let mut loaded_accounts = AHashMap::with_capacity(capacity);

        // SlotHistory may be overridden for simulation.
        // No other uses of AccountOverrides are expected.
        if let Some(slot_history) =
            account_overrides.and_then(|overrides| overrides.get(&slot_history::id()))
        {
            loaded_accounts.insert(slot_history::id(), slot_history.clone());
        }

        Self {
            loaded_accounts,
            callbacks,
            feature_set,
        }
    }

    // Load an account either from our own store or accounts-db and inspect it on behalf of Bank.
    // Inspection is required prior to any modifications to the account. This function is used
    // by load_transaction() and validate_transaction_fee_payer() for that purpose. It returns
    // a different type than other AccountLoader load functions, which should prevent accidental
    // mix and match of them.
    pub(crate) fn load_transaction_account(
        &mut self,
        account_key: &Pubkey,
        is_writable: bool,
    ) -> Option<LoadedTransactionAccount> {
        let base_account_size = if self.feature_set.formalize_loaded_transaction_data_size {
            TRANSACTION_ACCOUNT_BASE_SIZE
        } else {
            0
        };

        let account = self.load_account(account_key);

        // Inspect prior to collecting rent, since rent collection can modify
        // the account.
        //
        // Note that though rent collection is disabled, we still set the rent
        // epoch of rent exempt if the account is rent-exempt but its rent epoch
        // is not set to u64::MAX. In other words, an account can be updated
        // during rent collection. Therefore, we must inspect prior to collecting rent.
        self.callbacks.inspect_account(
            account_key,
            if let Some(ref account) = account {
                AccountState::Alive(account)
            } else {
                AccountState::Dead
            },
            is_writable,
        );

        account.map(|account| LoadedTransactionAccount {
            loaded_size: base_account_size.saturating_add(account.data().len()),
            account,
        })
    }

    // Load an account as above, with no inspection and no LoadedTransactionAccount wrapper.
    // This is a general purpose function suitable for usage outside initial transaction loading.
    pub(crate) fn load_account(&mut self, account_key: &Pubkey) -> Option<AccountSharedData> {
        match self.do_load(account_key) {
            // Exists, from AccountLoader.
            (Some(account), false) => Some(account),
            // Not allocated, but has an AccountLoader placeholder already.
            (None, false) => None,
            // Exists in accounts-db. Store it in AccountLoader for future loads.
            (Some(account), true) => {
                self.loaded_accounts.insert(*account_key, account.clone());
                Some(account)
            }
            // Does not exist and has never been seen.
            (None, true) => {
                self.loaded_accounts
                    .insert(*account_key, AccountSharedData::default());
                None
            }
        }
    }

    // Internal helper for core loading logic to prevent code duplication. Returns a bool
    // indicating whether an accounts-db lookup was performed, which allows wrappers with
    // &mut self to insert the account. Wrappers with &self ignore it.
    fn do_load(&self, account_key: &Pubkey) -> (Option<AccountSharedData>, bool) {
        if let Some(account) = self.loaded_accounts.get(account_key) {
            // If lamports is 0, a previous transaction deallocated this account.
            // We return None instead of the account we found so it can be created fresh.
            // We *never* remove accounts, or else we would fetch stale state from accounts-db.
            let option_account = if account.lamports() == 0 {
                None
            } else {
                Some(account.clone())
            };

            (option_account, false)
        } else if let Some((account, _slot)) = self.callbacks.get_account_shared_data(account_key) {
            (Some(account), true)
        } else {
            (None, true)
        }
    }

    pub(crate) fn update_accounts_for_failed_tx(&mut self, rollback_accounts: &RollbackAccounts) {
        for (account_address, account) in rollback_accounts {
            self.loaded_accounts
                .insert(*account_address, account.clone());
        }
    }

    pub(crate) fn update_accounts_for_successful_tx(
        &mut self,
        message: &impl SVMMessage,
        transaction_accounts: &[KeyedAccountSharedData],
    ) {
        for (i, (address, account)) in (0..message.account_keys().len()).zip(transaction_accounts) {
            if !message.is_writable(i) {
                continue;
            }

            // Accounts that are invoked and also not passed as an instruction
            // account to a program don't need to be stored because it's assumed
            // to be impossible for a committable transaction to modify an
            // invoked account if said account isn't passed to some program.
            if message.is_invoked(i) && !message.is_instruction_account(i) {
                continue;
            }

            self.loaded_accounts.insert(*address, account.clone());
        }
    }
}

// Program loaders and parsers require a type that impls TransactionProcessingCallback,
// because they are used in both SVM and by Bank. We impl it, with the consequence
// that if we fall back to accounts-db, we cannot store the state for future loads.
// In general, most accounts we load this way should already be in our accounts store.
// Once SIMD-0186 is implemented, 100% of accounts will be.
impl<CB: TransactionProcessingCallback> TransactionProcessingCallback for AccountLoader<'_, CB> {
    fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<(AccountSharedData, Slot)> {
        // The returned last-modification-slot is a dummy value for now,
        // but will later be used in IndexImplementation::V2 of the global program cache.
        self.do_load(pubkey).0.map(|account| (account, 0))
    }
}

// NOTE this is a required subtrait of TransactionProcessingCallback.
// It may make sense to break out a second subtrait just for the above two functions,
// but this would be a nontrivial breaking change and require careful consideration.
impl<CB: TransactionProcessingCallback> solana_svm_callback::InvokeContextCallback
    for AccountLoader<'_, CB>
{
}

/// Set the rent epoch to u64::MAX if the account is rent exempt.
///
/// TODO: This function is used to update the rent epoch of an account. Once we
/// completely switched to lthash, where rent_epoch is ignored in accounts
/// hashing, we can remove this function.
pub fn update_rent_exempt_status_for_account(rent: &Rent, account: &mut AccountSharedData) {
    // Now that rent fee collection is disabled, we won't collect rent for any
    // account. If there are any rent paying accounts, their `rent_epoch` won't
    // change either. However, if the account itself is rent-exempted but its
    // `rent_epoch` is not u64::MAX, we will set its `rent_epoch` to u64::MAX.
    // In such case, the behavior stays the same as before.
    if account.rent_epoch() != RENT_EXEMPT_RENT_EPOCH
        && rent.is_exempt(account.lamports(), account.data().len())
    {
        account.set_rent_epoch(RENT_EXEMPT_RENT_EPOCH);
    }
}

/// Check whether the payer_account is capable of paying the fee. The
/// side effect is to subtract the fee amount from the payer_account
/// balance of lamports. If the payer_account is not able to pay the
/// fee, the error_metrics is incremented, and a specific error is
/// returned.
pub fn validate_fee_payer(
    payer_address: &Pubkey,
    payer_account: &mut AccountSharedData,
    payer_index: IndexOfAccount,
    error_metrics: &mut TransactionErrorMetrics,
    rent: &Rent,
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
            rent.minimum_balance(NonceState::size())
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

    let payer_pre_rent_state =
        get_account_rent_state(rent, payer_account.lamports(), payer_account.data().len());
    payer_account
        .checked_sub_lamports(fee)
        .map_err(|_| TransactionError::InsufficientFundsForFee)?;

    let payer_post_rent_state =
        get_account_rent_state(rent, payer_account.lamports(), payer_account.data().len());
    check_rent_state_with_account(
        &payer_pre_rent_state,
        &payer_post_rent_state,
        payer_address,
        payer_index,
    )
}

pub(crate) fn load_transaction<CB: TransactionProcessingCallback>(
    account_loader: &mut AccountLoader<CB>,
    message: &impl SVMMessage,
    validation_result: TransactionValidationResult,
    error_metrics: &mut TransactionErrorMetrics,
    rent: &Rent,
) -> TransactionLoadResult {
    match validation_result {
        Err(e) => TransactionLoadResult::NotLoaded(e),
        Ok(tx_details) => {
            let load_result = load_transaction_accounts(
                account_loader,
                message,
                tx_details.loaded_fee_payer_account,
                tx_details.loaded_accounts_bytes_limit,
                error_metrics,
                rent,
            );

            match load_result {
                Ok(loaded_tx_accounts) => TransactionLoadResult::Loaded(LoadedTransaction {
                    accounts: loaded_tx_accounts.accounts,
                    program_indices: loaded_tx_accounts.program_indices,
                    fee_details: tx_details.fee_details,
                    rollback_accounts: tx_details.rollback_accounts,
                    compute_budget: tx_details.compute_budget,
                    loaded_accounts_data_size: loaded_tx_accounts.loaded_accounts_data_size,
                }),
                Err(err) => TransactionLoadResult::FeesOnly(FeesOnlyTransaction {
                    load_error: err,
                    fee_details: tx_details.fee_details,
                    rollback_accounts: tx_details.rollback_accounts,
                }),
            }
        }
    }
}

#[derive(PartialEq, Eq, Debug, Clone)]
struct LoadedTransactionAccounts {
    pub(crate) accounts: Vec<KeyedAccountSharedData>,
    pub(crate) program_indices: Vec<IndexOfAccount>,
    pub(crate) loaded_accounts_data_size: u32,
}

impl LoadedTransactionAccounts {
    fn increase_calculated_data_size(
        &mut self,
        data_size_delta: usize,
        requested_loaded_accounts_data_size_limit: NonZeroU32,
        error_metrics: &mut TransactionErrorMetrics,
    ) -> Result<()> {
        let Ok(data_size_delta) = u32::try_from(data_size_delta) else {
            error_metrics.max_loaded_accounts_data_size_exceeded += 1;
            return Err(TransactionError::MaxLoadedAccountsDataSizeExceeded);
        };

        self.loaded_accounts_data_size = self
            .loaded_accounts_data_size
            .saturating_add(data_size_delta);

        if self.loaded_accounts_data_size > requested_loaded_accounts_data_size_limit.get() {
            error_metrics.max_loaded_accounts_data_size_exceeded += 1;
            Err(TransactionError::MaxLoadedAccountsDataSizeExceeded)
        } else {
            Ok(())
        }
    }
}

fn load_transaction_accounts<CB: TransactionProcessingCallback>(
    account_loader: &mut AccountLoader<CB>,
    message: &impl SVMMessage,
    loaded_fee_payer_account: LoadedTransactionAccount,
    loaded_accounts_bytes_limit: NonZeroU32,
    error_metrics: &mut TransactionErrorMetrics,
    rent: &Rent,
) -> Result<LoadedTransactionAccounts> {
    if account_loader
        .feature_set
        .formalize_loaded_transaction_data_size
    {
        load_transaction_accounts_simd186(
            account_loader,
            message,
            loaded_fee_payer_account,
            loaded_accounts_bytes_limit,
            error_metrics,
            rent,
        )
    } else {
        load_transaction_accounts_old(
            account_loader,
            message,
            loaded_fee_payer_account,
            loaded_accounts_bytes_limit,
            error_metrics,
            rent,
        )
    }
}

fn load_transaction_accounts_simd186<CB: TransactionProcessingCallback>(
    account_loader: &mut AccountLoader<CB>,
    message: &impl SVMMessage,
    loaded_fee_payer_account: LoadedTransactionAccount,
    loaded_accounts_bytes_limit: NonZeroU32,
    error_metrics: &mut TransactionErrorMetrics,
    rent: &Rent,
) -> Result<LoadedTransactionAccounts> {
    let account_keys = message.account_keys();
    let mut additional_loaded_accounts: AHashSet<Pubkey> = AHashSet::new();

    let mut loaded_transaction_accounts = LoadedTransactionAccounts {
        accounts: Vec::with_capacity(account_keys.len()),
        program_indices: Vec::with_capacity(message.num_instructions()),
        loaded_accounts_data_size: 0,
    };

    // Transactions pay a base fee per address lookup table.
    loaded_transaction_accounts.increase_calculated_data_size(
        message
            .num_lookup_tables()
            .saturating_mul(ADDRESS_LOOKUP_TABLE_BASE_SIZE),
        loaded_accounts_bytes_limit,
        error_metrics,
    )?;

    let mut collect_loaded_account =
        |account_loader: &mut AccountLoader<CB>, key: &Pubkey, loaded_account| -> Result<()> {
            let LoadedTransactionAccount {
                account,
                loaded_size,
            } = loaded_account;

            loaded_transaction_accounts.increase_calculated_data_size(
                loaded_size,
                loaded_accounts_bytes_limit,
                error_metrics,
            )?;

            // This has been annotated branch-by-branch because collapsing the logic is infeasible.
            // Its purpose is to ensure programdata accounts are counted once and *only* once per
            // transaction. By checking account_keys, we never double-count a programdata account
            // that was explicitly included in the transaction. We also use a hashset to gracefully
            // handle cases that LoaderV3 presumably makes impossible, such as self-referential
            // program accounts or multiply-referenced programdata accounts, for added safety.
            //
            // If in the future LoaderV3 programs are migrated to LoaderV4, this entire code block
            // can be deleted.
            //
            // If this is a valid LoaderV3 program...
            if bpf_loader_upgradeable::check_id(account.owner()) {
                if let Ok(UpgradeableLoaderState::Program {
                    programdata_address,
                }) = account.state()
                {
                    // ...its programdata was not already counted and will not later be counted...
                    if !account_keys.iter().any(|key| programdata_address == *key)
                        && !additional_loaded_accounts.contains(&programdata_address)
                    {
                        // ...and the programdata account exists (if it doesn't, it is *not* a load failure)...
                        if let Some(programdata_account) =
                            account_loader.load_account(&programdata_address)
                        {
                            // ...count programdata toward this transaction's total size.
                            loaded_transaction_accounts.increase_calculated_data_size(
                                TRANSACTION_ACCOUNT_BASE_SIZE
                                    .saturating_add(programdata_account.data().len()),
                                loaded_accounts_bytes_limit,
                                error_metrics,
                            )?;
                            additional_loaded_accounts.insert(programdata_address);
                        }
                    }
                }
            }

            loaded_transaction_accounts.accounts.push((*key, account));

            Ok(())
        };

    // Since the fee payer is always the first account, collect it first.
    // We can use it directly because it was already loaded during validation.
    collect_loaded_account(
        account_loader,
        message.fee_payer(),
        loaded_fee_payer_account,
    )?;

    // Attempt to load and collect remaining non-fee payer accounts.
    for (account_index, account_key) in account_keys.iter().enumerate().skip(1) {
        let loaded_account =
            load_transaction_account(account_loader, message, account_key, account_index, rent);
        collect_loaded_account(account_loader, account_key, loaded_account)?;
    }

    for (program_id, instruction) in message.program_instructions_iter() {
        let Some(program_account) = account_loader.load_account(program_id) else {
            error_metrics.account_not_found += 1;
            return Err(TransactionError::ProgramAccountNotFound);
        };

        let owner_id = program_account.owner();
        if !native_loader::check_id(owner_id) && !PROGRAM_OWNERS.contains(owner_id) {
            error_metrics.invalid_program_for_execution += 1;
            return Err(TransactionError::InvalidProgramForExecution);
        }

        loaded_transaction_accounts
            .program_indices
            .push(instruction.program_id_index as IndexOfAccount);
    }

    Ok(loaded_transaction_accounts)
}

fn load_transaction_accounts_old<CB: TransactionProcessingCallback>(
    account_loader: &mut AccountLoader<CB>,
    message: &impl SVMMessage,
    loaded_fee_payer_account: LoadedTransactionAccount,
    loaded_accounts_bytes_limit: NonZeroU32,
    error_metrics: &mut TransactionErrorMetrics,
    rent: &Rent,
) -> Result<LoadedTransactionAccounts> {
    let account_keys = message.account_keys();
    let mut accounts = Vec::with_capacity(account_keys.len());
    let mut validated_loaders = AHashSet::with_capacity(PROGRAM_OWNERS.len());
    let mut accumulated_accounts_data_size: Saturating<u32> = Saturating(0);

    let mut collect_loaded_account = |key: &Pubkey, loaded_account| -> Result<()> {
        let LoadedTransactionAccount {
            account,
            loaded_size,
        } = loaded_account;

        accumulate_and_check_loaded_account_data_size(
            &mut accumulated_accounts_data_size,
            loaded_size,
            loaded_accounts_bytes_limit,
            error_metrics,
        )?;

        accounts.push((*key, account));
        Ok(())
    };

    // Since the fee payer is always the first account, collect it first.
    // We can use it directly because it was already loaded during validation.
    collect_loaded_account(message.fee_payer(), loaded_fee_payer_account)?;

    // Attempt to load and collect remaining non-fee payer accounts
    for (account_index, account_key) in account_keys.iter().enumerate().skip(1) {
        let loaded_account =
            load_transaction_account(account_loader, message, account_key, account_index, rent);
        collect_loaded_account(account_key, loaded_account)?;
    }

    let program_indices = message
        .program_instructions_iter()
        .map(|(program_id, instruction)| {
            if native_loader::check_id(program_id) {
                // Just as with an empty vector, trying to borrow the program account will fail
                // with a u16::MAX
                return Ok(u16::MAX as IndexOfAccount);
            }

            let program_index = instruction.program_id_index as usize;

            let Some(program_account) = account_loader.load_account(program_id) else {
                error_metrics.account_not_found += 1;
                return Err(TransactionError::ProgramAccountNotFound);
            };

            let owner_id = program_account.owner();
            if native_loader::check_id(owner_id) {
                return Ok(program_index as IndexOfAccount);
            }

            if !validated_loaders.contains(owner_id) {
                if let Some(owner_account) = account_loader.load_account(owner_id) {
                    if !native_loader::check_id(owner_account.owner()) {
                        error_metrics.invalid_program_for_execution += 1;
                        return Err(TransactionError::InvalidProgramForExecution);
                    }
                    accumulate_and_check_loaded_account_data_size(
                        &mut accumulated_accounts_data_size,
                        owner_account.data().len(),
                        loaded_accounts_bytes_limit,
                        error_metrics,
                    )?;
                    validated_loaders.insert(*owner_id);
                } else {
                    error_metrics.account_not_found += 1;
                    return Err(TransactionError::ProgramAccountNotFound);
                }
            }
            Ok(program_index as IndexOfAccount)
        })
        .collect::<Result<Vec<IndexOfAccount>>>()?;

    Ok(LoadedTransactionAccounts {
        accounts,
        program_indices,
        loaded_accounts_data_size: accumulated_accounts_data_size.0,
    })
}

fn load_transaction_account<CB: TransactionProcessingCallback>(
    account_loader: &mut AccountLoader<CB>,
    message: &impl SVMMessage,
    account_key: &Pubkey,
    account_index: usize,
    rent: &Rent,
) -> LoadedTransactionAccount {
    let is_writable = message.is_writable(account_index);
    let loaded_account = if solana_sdk_ids::sysvar::instructions::check_id(account_key) {
        // Since the instructions sysvar is constructed by the SVM and modified
        // for each transaction instruction, it cannot be loaded.
        LoadedTransactionAccount {
            loaded_size: 0,
            account: construct_instructions_account(message),
        }
    } else if let Some(mut loaded_account) =
        account_loader.load_transaction_account(account_key, is_writable)
    {
        if is_writable {
            update_rent_exempt_status_for_account(rent, &mut loaded_account.account);
        }
        loaded_account
    } else {
        let mut default_account = AccountSharedData::default();
        default_account.set_rent_epoch(RENT_EXEMPT_RENT_EPOCH);
        LoadedTransactionAccount {
            loaded_size: default_account.data().len(),
            account: default_account,
        }
    };

    loaded_account
}

/// Accumulate loaded account data size into `accumulated_accounts_data_size`.
/// Returns TransactionErr::MaxLoadedAccountsDataSizeExceeded if
/// `accumulated_accounts_data_size` exceeds
/// `requested_loaded_accounts_data_size_limit`.
fn accumulate_and_check_loaded_account_data_size(
    accumulated_loaded_accounts_data_size: &mut Saturating<u32>,
    account_data_size: usize,
    requested_loaded_accounts_data_size_limit: NonZeroU32,
    error_metrics: &mut TransactionErrorMetrics,
) -> Result<()> {
    let Ok(account_data_size) = u32::try_from(account_data_size) else {
        error_metrics.max_loaded_accounts_data_size_exceeded += 1;
        return Err(TransactionError::MaxLoadedAccountsDataSizeExceeded);
    };
    *accumulated_loaded_accounts_data_size += account_data_size;
    if accumulated_loaded_accounts_data_size.0 > requested_loaded_accounts_data_size_limit.get() {
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
        crate::transaction_account_state_info::TransactionAccountStateInfo,
        rand0_7::prelude::*,
        solana_account::{Account, AccountSharedData, ReadableAccount, WritableAccount},
        solana_hash::Hash,
        solana_instruction::{AccountMeta, Instruction},
        solana_keypair::Keypair,
        solana_loader_v3_interface::state::UpgradeableLoaderState,
        solana_message::{
            compiled_instruction::CompiledInstruction,
            v0::{LoadedAddresses, LoadedMessage},
            LegacyMessage, Message, MessageHeader, SanitizedMessage,
        },
        solana_native_token::LAMPORTS_PER_SOL,
        solana_nonce::{self as nonce, versions::Versions as NonceVersions},
        solana_program_runtime::execution_budget::{
            DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT, MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
        },
        solana_pubkey::Pubkey,
        solana_rent::Rent,
        solana_sdk_ids::{
            bpf_loader, bpf_loader_upgradeable, native_loader, system_program, sysvar,
        },
        solana_signature::Signature,
        solana_signer::Signer,
        solana_svm_callback::{InvokeContextCallback, TransactionProcessingCallback},
        solana_system_transaction::transfer,
        solana_transaction::{sanitized::SanitizedTransaction, Transaction},
        solana_transaction_context::{
            transaction_accounts::KeyedAccountSharedData, TransactionContext,
        },
        solana_transaction_error::{TransactionError, TransactionResult as Result},
        std::{
            borrow::Cow,
            cell::RefCell,
            collections::{HashMap, HashSet},
            fs::File,
            io::Read,
            sync::Arc,
        },
        test_case::test_case,
    };

    #[derive(Clone)]
    struct TestCallbacks {
        accounts_map: HashMap<Pubkey, AccountSharedData>,
        #[allow(clippy::type_complexity)]
        inspected_accounts:
            RefCell<HashMap<Pubkey, Vec<(Option<AccountSharedData>, /* is_writable */ bool)>>>,
        feature_set: SVMFeatureSet,
    }

    impl Default for TestCallbacks {
        fn default() -> Self {
            Self {
                accounts_map: HashMap::default(),
                inspected_accounts: RefCell::default(),
                feature_set: SVMFeatureSet::all_enabled(),
            }
        }
    }

    impl InvokeContextCallback for TestCallbacks {}

    impl TransactionProcessingCallback for TestCallbacks {
        fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<(AccountSharedData, Slot)> {
            self.accounts_map
                .get(pubkey)
                .map(|account| (account.clone(), 0))
        }

        fn inspect_account(
            &self,
            address: &Pubkey,
            account_state: AccountState,
            is_writable: bool,
        ) {
            let account = match account_state {
                AccountState::Dead => None,
                AccountState::Alive(account) => Some(account.clone()),
            };
            self.inspected_accounts
                .borrow_mut()
                .entry(*address)
                .or_default()
                .push((account, is_writable));
        }
    }

    impl<'a> From<&'a TestCallbacks> for AccountLoader<'a, TestCallbacks> {
        fn from(callbacks: &'a TestCallbacks) -> AccountLoader<'a, TestCallbacks> {
            AccountLoader::new_with_loaded_accounts_capacity(
                None,
                callbacks,
                &callbacks.feature_set,
                0,
            )
        }
    }

    fn load_accounts_with_features_and_rent(
        tx: Transaction,
        accounts: &[KeyedAccountSharedData],
        rent: &Rent,
        error_metrics: &mut TransactionErrorMetrics,
        feature_set: SVMFeatureSet,
    ) -> TransactionLoadResult {
        let sanitized_tx = SanitizedTransaction::from_transaction_for_tests(tx);
        let fee_payer_account = accounts[0].1.clone();
        let mut accounts_map = HashMap::new();
        for (pubkey, account) in accounts {
            accounts_map.insert(*pubkey, account.clone());
        }
        let callbacks = TestCallbacks {
            accounts_map,
            ..Default::default()
        };
        let mut account_loader: AccountLoader<TestCallbacks> = (&callbacks).into();
        account_loader.feature_set = &feature_set;
        load_transaction(
            &mut account_loader,
            &sanitized_tx,
            Ok(ValidatedTransactionDetails {
                loaded_fee_payer_account: LoadedTransactionAccount {
                    account: fee_payer_account,
                    ..LoadedTransactionAccount::default()
                },
                ..ValidatedTransactionDetails::default()
            }),
            error_metrics,
            rent,
        )
    }

    fn new_unchecked_sanitized_message(message: Message) -> SanitizedMessage {
        SanitizedMessage::Legacy(LegacyMessage::new(message, &HashSet::new()))
    }

    #[test_case(false; "informal_loaded_size")]
    #[test_case(true; "simd186_loaded_size")]
    fn test_load_accounts_unknown_program_id(formalize_loaded_transaction_data_size: bool) {
        let mut accounts: Vec<KeyedAccountSharedData> = Vec::new();
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

        let mut feature_set = SVMFeatureSet::all_enabled();
        feature_set.formalize_loaded_transaction_data_size = formalize_loaded_transaction_data_size;

        let load_results = load_accounts_with_features_and_rent(
            tx,
            &accounts,
            &Rent::default(),
            &mut error_metrics,
            feature_set,
        );

        assert_eq!(error_metrics.account_not_found.0, 1);
        assert!(matches!(
            load_results,
            TransactionLoadResult::FeesOnly(FeesOnlyTransaction {
                load_error: TransactionError::ProgramAccountNotFound,
                ..
            }),
        ));
    }

    #[test_case(false; "informal_loaded_size")]
    #[test_case(true; "simd186_loaded_size")]
    fn test_load_accounts_no_loaders(formalize_loaded_transaction_data_size: bool) {
        let mut accounts: Vec<KeyedAccountSharedData> = Vec::new();
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

        let mut feature_set = SVMFeatureSet::all_enabled();
        feature_set.formalize_loaded_transaction_data_size = formalize_loaded_transaction_data_size;

        let loaded_accounts = load_accounts_with_features_and_rent(
            tx,
            &accounts,
            &Rent::default(),
            &mut error_metrics,
            feature_set,
        );

        match &loaded_accounts {
            TransactionLoadResult::Loaded(loaded_transaction)
                if !formalize_loaded_transaction_data_size =>
            {
                assert_eq!(error_metrics.account_not_found.0, 0);
                assert_eq!(loaded_transaction.accounts.len(), 3);
                assert_eq!(loaded_transaction.accounts[0].1, accounts[0].1);
                assert_eq!(loaded_transaction.program_indices.len(), 1);
                assert_eq!(loaded_transaction.program_indices[0], u16::MAX);
            }
            TransactionLoadResult::FeesOnly(fees_only_tx)
                if formalize_loaded_transaction_data_size =>
            {
                assert_eq!(error_metrics.account_not_found.0, 1);
                assert_eq!(
                    fees_only_tx.load_error,
                    TransactionError::ProgramAccountNotFound,
                );
            }
            result => panic!("unexpected result: {result:?}"),
        }
    }

    #[test_case(false; "informal_loaded_size")]
    #[test_case(true; "simd186_loaded_size")]
    fn test_load_accounts_bad_owner(formalize_loaded_transaction_data_size: bool) {
        let mut accounts: Vec<KeyedAccountSharedData> = Vec::new();
        let mut error_metrics = TransactionErrorMetrics::default();

        let keypair = Keypair::new();
        let key0 = keypair.pubkey();
        let key1 = Pubkey::from([5u8; 32]);

        let account = AccountSharedData::new(1, 0, &Pubkey::default());
        accounts.push((key0, account));

        let mut account = AccountSharedData::new(40, 1, &Pubkey::default());
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

        let mut feature_set = SVMFeatureSet::all_enabled();
        feature_set.formalize_loaded_transaction_data_size = formalize_loaded_transaction_data_size;

        let load_results = load_accounts_with_features_and_rent(
            tx,
            &accounts,
            &Rent::default(),
            &mut error_metrics,
            feature_set,
        );

        if formalize_loaded_transaction_data_size {
            assert_eq!(error_metrics.invalid_program_for_execution.0, 1);
            assert!(matches!(
                load_results,
                TransactionLoadResult::FeesOnly(FeesOnlyTransaction {
                    load_error: TransactionError::InvalidProgramForExecution,
                    ..
                }),
            ));
        } else {
            assert_eq!(error_metrics.account_not_found.0, 1);
            assert!(matches!(
                load_results,
                TransactionLoadResult::FeesOnly(FeesOnlyTransaction {
                    load_error: TransactionError::ProgramAccountNotFound,
                    ..
                }),
            ));
        }
    }

    #[test_case(false; "informal_loaded_size")]
    #[test_case(true; "simd186_loaded_size")]
    fn test_load_accounts_not_executable(formalize_loaded_transaction_data_size: bool) {
        let mut accounts: Vec<KeyedAccountSharedData> = Vec::new();
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

        let mut feature_set = SVMFeatureSet::all_enabled();
        feature_set.formalize_loaded_transaction_data_size = formalize_loaded_transaction_data_size;

        let load_results = load_accounts_with_features_and_rent(
            tx,
            &accounts,
            &Rent::default(),
            &mut error_metrics,
            feature_set,
        );

        assert_eq!(error_metrics.invalid_program_for_execution.0, 0);
        match &load_results {
            TransactionLoadResult::Loaded(loaded_transaction) => {
                assert_eq!(loaded_transaction.accounts.len(), 2);
                assert_eq!(loaded_transaction.accounts[0].1, accounts[0].1);
                assert_eq!(loaded_transaction.accounts[1].1, accounts[1].1);
                assert_eq!(loaded_transaction.program_indices.len(), 1);
                assert_eq!(loaded_transaction.program_indices[0], 1);
            }
            TransactionLoadResult::FeesOnly(fees_only_tx) => panic!("{}", fees_only_tx.load_error),
            TransactionLoadResult::NotLoaded(e) => panic!("{e}"),
        }
    }

    #[test_case(false; "informal_loaded_size")]
    #[test_case(true; "simd186_loaded_size")]
    fn test_load_accounts_multiple_loaders(formalize_loaded_transaction_data_size: bool) {
        let mut accounts: Vec<KeyedAccountSharedData> = Vec::new();
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

        let mut feature_set = SVMFeatureSet::all_enabled();
        feature_set.formalize_loaded_transaction_data_size = formalize_loaded_transaction_data_size;

        let loaded_accounts = load_accounts_with_features_and_rent(
            tx,
            &accounts,
            &Rent::default(),
            &mut error_metrics,
            feature_set,
        );

        assert_eq!(error_metrics.account_not_found.0, 0);
        match &loaded_accounts {
            TransactionLoadResult::Loaded(loaded_transaction) => {
                assert_eq!(loaded_transaction.accounts.len(), 3);
                assert_eq!(loaded_transaction.accounts[0].1, accounts[0].1);
                assert_eq!(loaded_transaction.program_indices.len(), 2);
                assert_eq!(loaded_transaction.program_indices[0], 1);
                assert_eq!(loaded_transaction.program_indices[1], 2);
            }
            TransactionLoadResult::FeesOnly(fees_only_tx) => panic!("{}", fees_only_tx.load_error),
            TransactionLoadResult::NotLoaded(e) => panic!("{e}"),
        }
    }

    fn load_accounts_no_store(
        accounts: &[KeyedAccountSharedData],
        tx: Transaction,
        account_overrides: Option<&AccountOverrides>,
    ) -> TransactionLoadResult {
        let tx = SanitizedTransaction::from_transaction_for_tests(tx);

        let mut error_metrics = TransactionErrorMetrics::default();
        let mut accounts_map = HashMap::new();
        for (pubkey, account) in accounts {
            accounts_map.insert(*pubkey, account.clone());
        }
        let callbacks = TestCallbacks {
            accounts_map,
            ..Default::default()
        };
        let feature_set = SVMFeatureSet::all_enabled();
        let mut account_loader = AccountLoader::new_with_loaded_accounts_capacity(
            account_overrides,
            &callbacks,
            &feature_set,
            0,
        );
        load_transaction(
            &mut account_loader,
            &tx,
            Ok(ValidatedTransactionDetails::default()),
            &mut error_metrics,
            &Rent::default(),
        )
    }

    #[test]
    fn test_instructions() {
        agave_logger::setup();
        let instructions_key = solana_sdk_ids::sysvar::instructions::id();
        let keypair = Keypair::new();
        let instructions = vec![CompiledInstruction::new(1, &(), vec![0, 1])];
        let tx = Transaction::new_with_compiled_instructions(
            &[&keypair],
            &[solana_pubkey::new_rand(), instructions_key],
            Hash::default(),
            vec![native_loader::id()],
            instructions,
        );

        let load_results = load_accounts_no_store(&[], tx, None);
        assert!(matches!(
            load_results,
            TransactionLoadResult::FeesOnly(FeesOnlyTransaction {
                load_error: TransactionError::ProgramAccountNotFound,
                ..
            }),
        ));
    }

    #[test]
    fn test_overrides() {
        agave_logger::setup();
        let mut account_overrides = AccountOverrides::default();
        let slot_history_id = sysvar::slot_history::id();
        let account = AccountSharedData::new(42, 0, &Pubkey::default());
        account_overrides.set_slot_history(Some(account));

        let keypair = Keypair::new();
        let account = AccountSharedData::new(1_000_000, 0, &Pubkey::default());

        let mut program_account = AccountSharedData::default();
        program_account.set_lamports(1);
        program_account.set_executable(true);
        program_account.set_owner(native_loader::id());

        let instructions = vec![CompiledInstruction::new(2, &(), vec![0])];
        let tx = Transaction::new_with_compiled_instructions(
            &[&keypair],
            &[slot_history_id],
            Hash::default(),
            vec![bpf_loader::id()],
            instructions,
        );

        let loaded_accounts = load_accounts_no_store(
            &[
                (keypair.pubkey(), account),
                (bpf_loader::id(), program_account),
            ],
            tx,
            Some(&account_overrides),
        );
        match &loaded_accounts {
            TransactionLoadResult::Loaded(loaded_transaction) => {
                assert_eq!(loaded_transaction.accounts[0].0, keypair.pubkey());
                assert_eq!(loaded_transaction.accounts[1].0, slot_history_id);
                assert_eq!(loaded_transaction.accounts[1].1.lamports(), 42);
            }
            TransactionLoadResult::FeesOnly(fees_only_tx) => panic!("{}", fees_only_tx.load_error),
            TransactionLoadResult::NotLoaded(e) => panic!("{e}"),
        }
    }

    #[test]
    fn test_accumulate_and_check_loaded_account_data_size() {
        let mut error_metrics = TransactionErrorMetrics::default();
        let mut accumulated_data_size: Saturating<u32> = Saturating(0);
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
        assert_eq!(data_size as u32, accumulated_data_size.0);

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
    fn validate_fee_payer_account(test_parameter: ValidateFeePayerTestParameter, rent: &Rent) {
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
            rent,
            test_parameter.fee,
        );

        assert_eq!(result, test_parameter.expected_result);
        assert_eq!(account.lamports(), test_parameter.payer_post_balance);
    }

    #[test]
    fn test_validate_fee_payer() {
        let rent = Rent {
            lamports_per_byte_year: 1,
            ..Rent::default()
        };
        let min_balance = rent.minimum_balance(NonceState::size());
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
                    &rent,
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
                    &rent,
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
                    &rent,
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
                &rent,
            );
        }
    }

    #[test]
    fn test_validate_nonce_fee_payer_with_checked_arithmetic() {
        let rent = Rent {
            lamports_per_byte_year: 1,
            ..Rent::default()
        };

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
            &rent,
        );
    }

    #[test]
    fn test_construct_instructions_account() {
        let loaded_message = LoadedMessage {
            message: Cow::Owned(solana_message::v0::Message::default()),
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
    fn test_load_transaction_accounts_fee_payer() {
        let fee_payer_address = Pubkey::new_unique();
        let message = Message {
            account_keys: vec![fee_payer_address],
            header: MessageHeader::default(),
            instructions: vec![],
            recent_blockhash: Hash::default(),
        };

        let sanitized_message = new_unchecked_sanitized_message(message);
        let mut mock_bank = TestCallbacks::default();

        let fee_payer_balance = 200;
        let mut fee_payer_account = AccountSharedData::default();
        fee_payer_account.set_lamports(fee_payer_balance);
        mock_bank
            .accounts_map
            .insert(fee_payer_address, fee_payer_account.clone());
        let mut account_loader = (&mock_bank).into();

        let mut error_metrics = TransactionErrorMetrics::default();

        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );
        let result = load_transaction_accounts(
            &mut account_loader,
            sanitized_transaction.message(),
            LoadedTransactionAccount {
                loaded_size: fee_payer_account.data().len(),
                account: fee_payer_account.clone(),
            },
            MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
            &mut error_metrics,
            &Rent::default(),
        );
        assert_eq!(
            result.unwrap(),
            LoadedTransactionAccounts {
                accounts: vec![(fee_payer_address, fee_payer_account)],
                program_indices: vec![],
                loaded_accounts_data_size: 0,
            }
        );
    }

    #[test_case(false; "informal_loaded_size")]
    #[test_case(true; "simd186_loaded_size")]
    fn test_load_transaction_accounts_native_loader(formalize_loaded_transaction_data_size: bool) {
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
        mock_bank.feature_set.formalize_loaded_transaction_data_size =
            formalize_loaded_transaction_data_size;
        mock_bank
            .accounts_map
            .insert(native_loader::id(), AccountSharedData::default());
        let mut fee_payer_account = AccountSharedData::default();
        fee_payer_account.set_lamports(200);
        mock_bank
            .accounts_map
            .insert(key1.pubkey(), fee_payer_account.clone());
        let mut account_loader = (&mock_bank).into();

        let mut error_metrics = TransactionErrorMetrics::default();

        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );

        let base_account_size = if formalize_loaded_transaction_data_size {
            TRANSACTION_ACCOUNT_BASE_SIZE
        } else {
            0
        };

        let result = load_transaction_accounts(
            &mut account_loader,
            sanitized_transaction.message(),
            LoadedTransactionAccount {
                account: fee_payer_account.clone(),
                loaded_size: base_account_size,
            },
            MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
            &mut error_metrics,
            &Rent::default(),
        );

        if formalize_loaded_transaction_data_size {
            assert_eq!(
                result.unwrap_err(),
                TransactionError::ProgramAccountNotFound,
            );
        } else {
            let loaded_accounts_data_size = base_account_size as u32 * 2;
            assert_eq!(
                result.unwrap(),
                LoadedTransactionAccounts {
                    accounts: vec![
                        (key1.pubkey(), fee_payer_account),
                        (
                            native_loader::id(),
                            mock_bank.accounts_map[&native_loader::id()].clone()
                        )
                    ],
                    program_indices: vec![u16::MAX],
                    loaded_accounts_data_size,
                }
            );
        }
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
        mock_bank.accounts_map.insert(key1.pubkey(), account_data);
        let mut account_loader = (&mock_bank).into();

        let mut error_metrics = TransactionErrorMetrics::default();

        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );
        let result = load_transaction_accounts(
            &mut account_loader,
            sanitized_transaction.message(),
            LoadedTransactionAccount::default(),
            MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
            &mut error_metrics,
            &Rent::default(),
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
        mock_bank.accounts_map.insert(key1.pubkey(), account_data);
        let mut account_loader = (&mock_bank).into();

        let mut error_metrics = TransactionErrorMetrics::default();

        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );
        let result = load_transaction_accounts(
            &mut account_loader,
            sanitized_transaction.message(),
            LoadedTransactionAccount::default(),
            MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
            &mut error_metrics,
            &Rent::default(),
        );

        assert_eq!(
            result.err(),
            Some(TransactionError::InvalidProgramForExecution)
        );
    }

    #[test_case(false; "informal_loaded_size")]
    #[test_case(true; "simd186_loaded_size")]
    fn test_load_transaction_accounts_native_loader_owner(
        formalize_loaded_transaction_data_size: bool,
    ) {
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
        mock_bank.feature_set.formalize_loaded_transaction_data_size =
            formalize_loaded_transaction_data_size;
        let mut account_data = AccountSharedData::default();
        account_data.set_owner(native_loader::id());
        account_data.set_lamports(1);
        account_data.set_executable(true);
        mock_bank.accounts_map.insert(key1.pubkey(), account_data);

        let mut fee_payer_account = AccountSharedData::default();
        fee_payer_account.set_lamports(200);
        mock_bank
            .accounts_map
            .insert(key2.pubkey(), fee_payer_account.clone());
        let mut account_loader = (&mock_bank).into();

        let mut error_metrics = TransactionErrorMetrics::default();

        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );

        let base_account_size = if formalize_loaded_transaction_data_size {
            TRANSACTION_ACCOUNT_BASE_SIZE
        } else {
            0
        };

        let result = load_transaction_accounts(
            &mut account_loader,
            sanitized_transaction.message(),
            LoadedTransactionAccount {
                account: fee_payer_account.clone(),
                loaded_size: base_account_size,
            },
            MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
            &mut error_metrics,
            &Rent::default(),
        );

        let loaded_accounts_data_size = base_account_size as u32 * 2;

        assert_eq!(
            result.unwrap(),
            LoadedTransactionAccounts {
                accounts: vec![
                    (key2.pubkey(), fee_payer_account),
                    (
                        key1.pubkey(),
                        mock_bank.accounts_map[&key1.pubkey()].clone()
                    ),
                ],
                program_indices: vec![1],
                loaded_accounts_data_size,
            }
        );
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
        mock_bank.accounts_map.insert(key1.pubkey(), account_data);

        let mut account_data = AccountSharedData::default();
        account_data.set_lamports(200);
        mock_bank.accounts_map.insert(key2.pubkey(), account_data);
        let mut account_loader = (&mock_bank).into();

        let mut error_metrics = TransactionErrorMetrics::default();

        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );
        let result = load_transaction_accounts(
            &mut account_loader,
            sanitized_transaction.message(),
            LoadedTransactionAccount::default(),
            MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
            &mut error_metrics,
            &Rent::default(),
        );

        assert_eq!(result.err(), Some(TransactionError::ProgramAccountNotFound));
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
        account_data.set_lamports(1);
        account_data.set_executable(true);
        account_data.set_owner(key3.pubkey());
        mock_bank.accounts_map.insert(key1.pubkey(), account_data);

        let mut account_data = AccountSharedData::default();
        account_data.set_lamports(200);
        mock_bank.accounts_map.insert(key2.pubkey(), account_data);
        mock_bank
            .accounts_map
            .insert(key3.pubkey(), AccountSharedData::default());
        let mut account_loader = (&mock_bank).into();

        let mut error_metrics = TransactionErrorMetrics::default();

        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );
        let result = load_transaction_accounts(
            &mut account_loader,
            sanitized_transaction.message(),
            LoadedTransactionAccount::default(),
            MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
            &mut error_metrics,
            &Rent::default(),
        );

        assert_eq!(
            result.err(),
            Some(TransactionError::InvalidProgramForExecution)
        );
    }

    #[test_case(false; "informal_loaded_size")]
    #[test_case(true; "simd186_loaded_size")]
    fn test_load_transaction_accounts_program_success_complete(
        formalize_loaded_transaction_data_size: bool,
    ) {
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
        mock_bank.feature_set.formalize_loaded_transaction_data_size =
            formalize_loaded_transaction_data_size;
        let mut account_data = AccountSharedData::default();
        account_data.set_lamports(1);
        account_data.set_executable(true);
        account_data.set_owner(bpf_loader::id());
        mock_bank.accounts_map.insert(key1.pubkey(), account_data);

        let mut fee_payer_account = AccountSharedData::default();
        fee_payer_account.set_lamports(200);
        mock_bank
            .accounts_map
            .insert(key2.pubkey(), fee_payer_account.clone());

        let mut account_data = AccountSharedData::default();
        account_data.set_lamports(1);
        account_data.set_executable(true);
        account_data.set_owner(native_loader::id());
        mock_bank
            .accounts_map
            .insert(bpf_loader::id(), account_data);
        let mut account_loader = (&mock_bank).into();

        let mut error_metrics = TransactionErrorMetrics::default();

        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );

        let base_account_size = if formalize_loaded_transaction_data_size {
            TRANSACTION_ACCOUNT_BASE_SIZE
        } else {
            0
        };

        let result = load_transaction_accounts(
            &mut account_loader,
            sanitized_transaction.message(),
            LoadedTransactionAccount {
                account: fee_payer_account.clone(),
                loaded_size: base_account_size,
            },
            MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
            &mut error_metrics,
            &Rent::default(),
        );

        let loaded_accounts_data_size = base_account_size as u32 * 2;

        assert_eq!(
            result.unwrap(),
            LoadedTransactionAccounts {
                accounts: vec![
                    (key2.pubkey(), fee_payer_account),
                    (
                        key1.pubkey(),
                        mock_bank.accounts_map[&key1.pubkey()].clone()
                    ),
                ],
                program_indices: vec![1],
                loaded_accounts_data_size,
            }
        );
    }

    #[test_case(false; "informal_loaded_size")]
    #[test_case(true; "simd186_loaded_size")]
    fn test_load_transaction_accounts_program_builtin_saturating_add(
        formalize_loaded_transaction_data_size: bool,
    ) {
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let key3 = Keypair::new();

        let message = Message {
            account_keys: vec![key2.pubkey(), key1.pubkey(), key3.pubkey()],
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
        mock_bank.feature_set.formalize_loaded_transaction_data_size =
            formalize_loaded_transaction_data_size;
        let mut account_data = AccountSharedData::default();
        account_data.set_lamports(1);
        account_data.set_executable(true);
        account_data.set_owner(bpf_loader::id());
        mock_bank.accounts_map.insert(key1.pubkey(), account_data);

        let mut fee_payer_account = AccountSharedData::default();
        fee_payer_account.set_lamports(200);
        mock_bank
            .accounts_map
            .insert(key2.pubkey(), fee_payer_account.clone());

        let mut account_data = AccountSharedData::default();
        account_data.set_lamports(1);
        account_data.set_executable(true);
        account_data.set_owner(native_loader::id());
        mock_bank
            .accounts_map
            .insert(bpf_loader::id(), account_data);
        let mut account_loader = (&mock_bank).into();

        let mut error_metrics = TransactionErrorMetrics::default();

        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );

        let base_account_size = if formalize_loaded_transaction_data_size {
            TRANSACTION_ACCOUNT_BASE_SIZE
        } else {
            0
        };

        let result = load_transaction_accounts(
            &mut account_loader,
            sanitized_transaction.message(),
            LoadedTransactionAccount {
                account: fee_payer_account.clone(),
                loaded_size: base_account_size,
            },
            MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
            &mut error_metrics,
            &Rent::default(),
        );

        let loaded_accounts_data_size = base_account_size as u32 * 2;

        let mut account_data = AccountSharedData::default();
        account_data.set_rent_epoch(RENT_EXEMPT_RENT_EPOCH);
        assert_eq!(
            result.unwrap(),
            LoadedTransactionAccounts {
                accounts: vec![
                    (key2.pubkey(), fee_payer_account),
                    (
                        key1.pubkey(),
                        mock_bank.accounts_map[&key1.pubkey()].clone()
                    ),
                    (key3.pubkey(), account_data),
                ],
                program_indices: vec![1, 1],
                loaded_accounts_data_size,
            }
        );
    }

    #[test]
    fn test_rent_state_list_len() {
        let mint_keypair = Keypair::new();
        let mut bank = TestCallbacks::default();
        let recipient = Pubkey::new_unique();
        let last_block_hash = Hash::new_unique();

        let mut system_data = AccountSharedData::default();
        system_data.set_lamports(1);
        system_data.set_executable(true);
        system_data.set_owner(native_loader::id());
        bank.accounts_map
            .insert(Pubkey::new_from_array([0u8; 32]), system_data);

        let mut mint_data = AccountSharedData::default();
        mint_data.set_lamports(2);
        bank.accounts_map.insert(mint_keypair.pubkey(), mint_data);
        bank.accounts_map
            .insert(recipient, AccountSharedData::default());
        let mut account_loader = (&bank).into();

        let tx = transfer(&mint_keypair, &recipient, LAMPORTS_PER_SOL, last_block_hash);
        let num_accounts = tx.message().account_keys.len();
        let sanitized_tx = SanitizedTransaction::from_transaction_for_tests(tx);
        let mut error_metrics = TransactionErrorMetrics::default();
        let load_result = load_transaction(
            &mut account_loader,
            &sanitized_tx.clone(),
            Ok(ValidatedTransactionDetails::default()),
            &mut error_metrics,
            &Rent::default(),
        );

        let TransactionLoadResult::Loaded(loaded_transaction) = load_result else {
            panic!("transaction loading failed");
        };

        let compute_budget = SVMTransactionExecutionBudget {
            compute_unit_limit: u64::from(DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT),
            ..SVMTransactionExecutionBudget::default()
        };
        let rent = Rent::default();
        let transaction_context = TransactionContext::new(
            loaded_transaction.accounts,
            rent.clone(),
            compute_budget.max_instruction_stack_depth,
            compute_budget.max_instruction_trace_length,
        );

        assert_eq!(
            TransactionAccountStateInfo::new(&transaction_context, sanitized_tx.message(), &rent,)
                .len(),
            num_accounts,
        );
    }

    #[test_case(false; "informal_loaded_size")]
    #[test_case(true; "simd186_loaded_size")]
    fn test_load_accounts_success(formalize_loaded_transaction_data_size: bool) {
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let key3 = Keypair::new();

        let message = Message {
            account_keys: vec![key2.pubkey(), key1.pubkey(), key3.pubkey()],
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
        mock_bank.feature_set.formalize_loaded_transaction_data_size =
            formalize_loaded_transaction_data_size;
        let mut account_data = AccountSharedData::default();
        account_data.set_lamports(1);
        account_data.set_executable(true);
        account_data.set_owner(bpf_loader::id());
        mock_bank.accounts_map.insert(key1.pubkey(), account_data);

        let mut fee_payer_account = AccountSharedData::default();
        fee_payer_account.set_lamports(200);
        mock_bank
            .accounts_map
            .insert(key2.pubkey(), fee_payer_account.clone());

        let mut account_data = AccountSharedData::default();
        account_data.set_lamports(1);
        account_data.set_executable(true);
        account_data.set_owner(native_loader::id());
        mock_bank
            .accounts_map
            .insert(bpf_loader::id(), account_data);
        let mut account_loader = (&mock_bank).into();

        let mut error_metrics = TransactionErrorMetrics::default();

        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );

        let base_account_size = if formalize_loaded_transaction_data_size {
            TRANSACTION_ACCOUNT_BASE_SIZE
        } else {
            0
        };

        let validation_result = Ok(ValidatedTransactionDetails {
            loaded_fee_payer_account: LoadedTransactionAccount {
                account: fee_payer_account,
                loaded_size: base_account_size,
            },
            ..ValidatedTransactionDetails::default()
        });

        let load_result = load_transaction(
            &mut account_loader,
            &sanitized_transaction,
            validation_result,
            &mut error_metrics,
            &Rent::default(),
        );

        let loaded_accounts_data_size = base_account_size as u32 * 2;

        let mut account_data = AccountSharedData::default();
        account_data.set_rent_epoch(RENT_EXEMPT_RENT_EPOCH);

        let TransactionLoadResult::Loaded(loaded_transaction) = load_result else {
            panic!("transaction loading failed");
        };
        assert_eq!(
            loaded_transaction,
            LoadedTransaction {
                accounts: vec![
                    (
                        key2.pubkey(),
                        mock_bank.accounts_map[&key2.pubkey()].clone()
                    ),
                    (
                        key1.pubkey(),
                        mock_bank.accounts_map[&key1.pubkey()].clone()
                    ),
                    (key3.pubkey(), account_data),
                ],
                program_indices: vec![1, 1],
                fee_details: FeeDetails::default(),
                rollback_accounts: RollbackAccounts::default(),
                compute_budget: SVMTransactionExecutionBudget::default(),
                loaded_accounts_data_size,
            }
        );
    }

    #[test]
    fn test_load_accounts_error() {
        let mock_bank = TestCallbacks::default();
        let mut account_loader = (&mock_bank).into();
        let rent = Rent::default();

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

        let validation_result = Ok(ValidatedTransactionDetails::default());
        let load_result = load_transaction(
            &mut account_loader,
            &sanitized_transaction,
            validation_result.clone(),
            &mut TransactionErrorMetrics::default(),
            &rent,
        );

        assert!(matches!(
            load_result,
            TransactionLoadResult::FeesOnly(FeesOnlyTransaction {
                load_error: TransactionError::ProgramAccountNotFound,
                ..
            }),
        ));

        let validation_result = Err(TransactionError::InvalidWritableAccount);

        let load_result = load_transaction(
            &mut account_loader,
            &sanitized_transaction,
            validation_result,
            &mut TransactionErrorMetrics::default(),
            &rent,
        );

        assert!(matches!(
            load_result,
            TransactionLoadResult::NotLoaded(TransactionError::InvalidWritableAccount),
        ));
    }

    #[test]
    fn test_update_rent_exempt_status_for_account() {
        let rent = Rent::default();

        let min_exempt_balance = rent.minimum_balance(0);
        let mut account = AccountSharedData::from(Account {
            lamports: min_exempt_balance,
            ..Account::default()
        });

        update_rent_exempt_status_for_account(&rent, &mut account);
        assert_eq!(account.rent_epoch(), RENT_EXEMPT_RENT_EPOCH);
    }

    #[test]
    fn test_update_rent_exempt_status_for_rent_paying_account() {
        let rent = Rent::default();

        let mut account = AccountSharedData::from(Account {
            lamports: 1,
            ..Account::default()
        });

        update_rent_exempt_status_for_account(&rent, &mut account);
        assert_eq!(account.rent_epoch(), 0);
        assert_eq!(account.lamports(), 1);
    }

    // Ensure `TransactionProcessingCallback::inspect_account()` is called when
    // loading accounts for transaction processing.
    #[test]
    fn test_inspect_account_non_fee_payer() {
        let mut mock_bank = TestCallbacks::default();

        let address0 = Pubkey::new_unique(); // <-- fee payer
        let address1 = Pubkey::new_unique(); // <-- initially alive
        let address2 = Pubkey::new_unique(); // <-- initially dead
        let address3 = Pubkey::new_unique(); // <-- program

        let mut account0 = AccountSharedData::default();
        account0.set_lamports(1_000_000_000);
        mock_bank.accounts_map.insert(address0, account0.clone());

        let mut account1 = AccountSharedData::default();
        account1.set_lamports(2_000_000_000);
        mock_bank.accounts_map.insert(address1, account1.clone());

        // account2 *not* added to the bank's accounts_map

        let mut account3 = AccountSharedData::default();
        account3.set_lamports(4_000_000_000);
        account3.set_executable(true);
        account3.set_owner(bpf_loader::id());
        mock_bank.accounts_map.insert(address3, account3.clone());
        let mut account_loader = (&mock_bank).into();

        let message = Message {
            account_keys: vec![address0, address1, address2, address3],
            header: MessageHeader::default(),
            instructions: vec![
                CompiledInstruction {
                    program_id_index: 3,
                    accounts: vec![0],
                    data: vec![],
                },
                CompiledInstruction {
                    program_id_index: 3,
                    accounts: vec![1, 2],
                    data: vec![],
                },
                CompiledInstruction {
                    program_id_index: 3,
                    accounts: vec![1],
                    data: vec![],
                },
            ],
            recent_blockhash: Hash::new_unique(),
        };
        let sanitized_message = new_unchecked_sanitized_message(message);
        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );
        let validation_result = Ok(ValidatedTransactionDetails {
            loaded_fee_payer_account: LoadedTransactionAccount {
                account: account0.clone(),
                ..LoadedTransactionAccount::default()
            },
            ..ValidatedTransactionDetails::default()
        });
        let _load_results = load_transaction(
            &mut account_loader,
            &sanitized_transaction,
            validation_result,
            &mut TransactionErrorMetrics::default(),
            &Rent::default(),
        );

        // ensure the loaded accounts are inspected
        let mut actual_inspected_accounts: Vec<_> = mock_bank
            .inspected_accounts
            .borrow()
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        actual_inspected_accounts.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        let mut expected_inspected_accounts = vec![
            // *not* key0, since it is loaded during fee payer validation
            (address1, vec![(Some(account1), true)]),
            (address2, vec![(None, true)]),
            (address3, vec![(Some(account3), false)]),
        ];
        expected_inspected_accounts.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        assert_eq!(actual_inspected_accounts, expected_inspected_accounts,);
    }

    #[test]
    fn test_load_transaction_accounts_data_sizes() {
        let mut mock_bank = TestCallbacks::default();
        mock_bank.feature_set.formalize_loaded_transaction_data_size = false;

        let loader_v2 = bpf_loader::id();
        let loader_v3 = bpf_loader_upgradeable::id();
        let program1_keypair = Keypair::new();
        let program1 = program1_keypair.pubkey();
        let program2 = Pubkey::new_unique();
        let programdata2 = Pubkey::new_unique();
        use solana_account::state_traits::StateMut;

        let program2_size = std::mem::size_of::<UpgradeableLoaderState>() as u32;
        let mut program2_account = AccountSharedData::default();
        program2_account.set_owner(loader_v3);
        program2_account.set_lamports(LAMPORTS_PER_SOL);
        program2_account.set_executable(true);
        program2_account.set_data(vec![0; program2_size as usize]);
        program2_account
            .set_state(&UpgradeableLoaderState::Program {
                programdata_address: programdata2,
            })
            .unwrap();
        mock_bank.accounts_map.insert(program2, program2_account);
        let mut programdata2_account = AccountSharedData::default();
        programdata2_account.set_owner(loader_v3);
        programdata2_account.set_lamports(LAMPORTS_PER_SOL);
        programdata2_account.set_data(vec![0; program2_size as usize]);
        programdata2_account
            .set_state(&UpgradeableLoaderState::ProgramData {
                slot: 0,
                upgrade_authority_address: None,
            })
            .unwrap();
        let mut programdata = programdata2_account.data().to_vec();
        let mut file =
            File::open("tests/example-programs/hello-solana/hello_solana_program.so").unwrap();
        file.read_to_end(&mut programdata).unwrap();
        let programdata2_size = programdata.len() as u32;
        programdata2_account.set_data(programdata);
        mock_bank
            .accounts_map
            .insert(programdata2, programdata2_account);

        let mut next_size = 1;
        let mut make_account = |pubkey, owner, executable| {
            let size = next_size;
            let account = AccountSharedData::create(
                LAMPORTS_PER_SOL,
                vec![0; size],
                owner,
                executable,
                u64::MAX,
            );

            mock_bank.accounts_map.insert(pubkey, account.clone());

            // accounts are counted at most twice
            // by multiplying account size by 4, we ensure all totals are unique
            next_size *= 4;

            (size as u32, account)
        };

        let fee_payer_keypair = Keypair::new();
        let fee_payer = fee_payer_keypair.pubkey();
        let (fee_payer_size, fee_payer_account) =
            make_account(fee_payer, system_program::id(), false);

        let account1 = Pubkey::new_unique();
        let (account1_size, _) = make_account(account1, program1, false);

        let account2 = Pubkey::new_unique();
        let (account2_size, _) = make_account(account2, program2, false);

        let (native_loader_size, _) = make_account(native_loader::id(), native_loader::id(), true);
        let (bpf_loader_size, _) = make_account(loader_v2, native_loader::id(), true);
        let (upgradeable_loader_size, _) = make_account(loader_v3, native_loader::id(), true);

        let (program1_size, _) = make_account(program1, loader_v2, true);

        let mut program_accounts = HashMap::new();
        program_accounts.insert(program1, (&loader_v2, 0));
        program_accounts.insert(program2, (&loader_v3, 0));
        let test_transaction_data_size = |transaction, expected_size| {
            let mut account_loader = AccountLoader::new_with_loaded_accounts_capacity(
                None,
                &mock_bank,
                &mock_bank.feature_set,
                0,
            );

            let loaded_transaction_accounts = load_transaction_accounts(
                &mut account_loader,
                &transaction,
                LoadedTransactionAccount {
                    account: fee_payer_account.clone(),
                    loaded_size: fee_payer_size as usize,
                },
                MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
                &mut TransactionErrorMetrics::default(),
                &Rent::default(),
            )
            .unwrap();

            assert_eq!(
                loaded_transaction_accounts.loaded_accounts_data_size,
                expected_size
            );
        };

        let test_data_size = |instructions: Vec<_>, expected_size| {
            let transaction = SanitizedTransaction::from_transaction_for_tests(
                Transaction::new_signed_with_payer(
                    &instructions,
                    Some(&fee_payer),
                    &[&fee_payer_keypair],
                    Hash::default(),
                ),
            );

            test_transaction_data_size(transaction, expected_size)
        };

        for account_meta in [AccountMeta::new, AccountMeta::new_readonly] {
            // one program plus loader
            let ixns = vec![Instruction::new_with_bytes(program1, &[], vec![])];
            test_data_size(ixns, program1_size + bpf_loader_size + fee_payer_size);

            // two programs, two loaders, two accounts
            let ixns = vec![
                Instruction::new_with_bytes(program1, &[], vec![account_meta(account1, false)]),
                Instruction::new_with_bytes(program2, &[], vec![account_meta(account2, false)]),
            ];
            test_data_size(
                ixns,
                account1_size
                    + account2_size
                    + program1_size
                    + program2_size
                    + bpf_loader_size
                    + upgradeable_loader_size
                    + fee_payer_size,
            );

            // program and loader counted once
            let ixns = vec![
                Instruction::new_with_bytes(program2, &[], vec![]),
                Instruction::new_with_bytes(program2, &[], vec![]),
            ];
            test_data_size(
                ixns,
                program2_size + upgradeable_loader_size + fee_payer_size,
            );

            // native loader not counted if loader
            let ixns = vec![Instruction::new_with_bytes(bpf_loader::id(), &[], vec![])];
            test_data_size(ixns, bpf_loader_size + fee_payer_size);

            // native loader counted if instruction
            let ixns = vec![Instruction::new_with_bytes(
                bpf_loader::id(),
                &[],
                vec![account_meta(native_loader::id(), false)],
            )];
            test_data_size(ixns, bpf_loader_size + native_loader_size + fee_payer_size);

            // native loader counted if invoked
            let ixns = vec![Instruction::new_with_bytes(
                native_loader::id(),
                &[],
                vec![],
            )];
            test_data_size(ixns, native_loader_size + fee_payer_size);

            // native loader counted once if invoked and instruction
            let ixns = vec![Instruction::new_with_bytes(
                native_loader::id(),
                &[],
                vec![account_meta(native_loader::id(), false)],
            )];
            test_data_size(ixns, native_loader_size + fee_payer_size);

            // loader counted twice if included in instruction
            let ixns = vec![Instruction::new_with_bytes(
                program2,
                &[],
                vec![account_meta(bpf_loader_upgradeable::id(), false)],
            )];
            test_data_size(
                ixns,
                upgradeable_loader_size * 2 + program2_size + fee_payer_size,
            );

            // loader counted twice even if included first
            let ixns = vec![
                Instruction::new_with_bytes(bpf_loader_upgradeable::id(), &[], vec![]),
                Instruction::new_with_bytes(program2, &[], vec![]),
            ];
            test_data_size(
                ixns,
                upgradeable_loader_size * 2 + program2_size + fee_payer_size,
            );

            // cover that case with multiple loaders to be sure
            let ixns = vec![
                Instruction::new_with_bytes(
                    program1,
                    &[],
                    vec![
                        account_meta(bpf_loader::id(), false),
                        account_meta(bpf_loader_upgradeable::id(), false),
                    ],
                ),
                Instruction::new_with_bytes(program2, &[], vec![account_meta(account1, false)]),
                Instruction::new_with_bytes(
                    bpf_loader_upgradeable::id(),
                    &[],
                    vec![account_meta(account1, false)],
                ),
            ];
            test_data_size(
                ixns,
                account1_size
                    + program1_size
                    + program2_size
                    + bpf_loader_size * 2
                    + upgradeable_loader_size * 2
                    + fee_payer_size,
            );

            // fee-payer counted once
            let ixns = vec![Instruction::new_with_bytes(
                program1,
                &[],
                vec![account_meta(fee_payer, false)],
            )];
            test_data_size(ixns, program1_size + bpf_loader_size + fee_payer_size);

            // programdata as instruction account counts it once
            let ixns = vec![Instruction::new_with_bytes(
                program2,
                &[],
                vec![account_meta(programdata2, false)],
            )];
            test_data_size(
                ixns,
                program2_size + programdata2_size + upgradeable_loader_size + fee_payer_size,
            );

            // program and programdata as instruction accounts behaves the same
            let ixns = vec![Instruction::new_with_bytes(
                program2,
                &[],
                vec![
                    account_meta(program2, false),
                    account_meta(programdata2, false),
                ],
            )];
            test_data_size(
                ixns,
                program2_size + programdata2_size + upgradeable_loader_size + fee_payer_size,
            );
        }
    }

    #[test]
    fn test_account_loader_wrappers() {
        let fee_payer = Pubkey::new_unique();
        let mut fee_payer_account = AccountSharedData::default();
        fee_payer_account.set_rent_epoch(u64::MAX);
        fee_payer_account.set_lamports(5000);

        let mut mock_bank = TestCallbacks::default();
        mock_bank
            .accounts_map
            .insert(fee_payer, fee_payer_account.clone());

        // test without stored account
        let mut account_loader: AccountLoader<_> = (&mock_bank).into();
        assert_eq!(
            account_loader
                .load_transaction_account(&fee_payer, false)
                .unwrap()
                .account,
            fee_payer_account
        );

        let mut account_loader: AccountLoader<_> = (&mock_bank).into();
        assert_eq!(
            account_loader
                .load_transaction_account(&fee_payer, true)
                .unwrap()
                .account,
            fee_payer_account
        );

        let mut account_loader: AccountLoader<_> = (&mock_bank).into();
        assert_eq!(
            account_loader.load_account(&fee_payer).unwrap(),
            fee_payer_account
        );

        let account_loader: AccountLoader<_> = (&mock_bank).into();
        assert_eq!(
            account_loader
                .get_account_shared_data(&fee_payer)
                .unwrap()
                .0,
            fee_payer_account
        );

        // test with stored account
        let mut account_loader: AccountLoader<_> = (&mock_bank).into();
        account_loader.load_account(&fee_payer).unwrap();

        assert_eq!(
            account_loader
                .load_transaction_account(&fee_payer, false)
                .unwrap()
                .account,
            fee_payer_account
        );
        assert_eq!(
            account_loader
                .load_transaction_account(&fee_payer, true)
                .unwrap()
                .account,
            fee_payer_account
        );
        assert_eq!(
            account_loader.load_account(&fee_payer).unwrap(),
            fee_payer_account
        );
        assert_eq!(
            account_loader
                .get_account_shared_data(&fee_payer)
                .unwrap()
                .0,
            fee_payer_account
        );

        // drop the account and ensure all deliver the updated state
        fee_payer_account.set_lamports(0);
        account_loader.update_accounts_for_failed_tx(&RollbackAccounts::FeePayerOnly {
            fee_payer: (fee_payer, fee_payer_account),
        });

        assert_eq!(
            account_loader.load_transaction_account(&fee_payer, false),
            None
        );
        assert_eq!(
            account_loader.load_transaction_account(&fee_payer, true),
            None
        );
        assert_eq!(account_loader.load_account(&fee_payer), None);
        assert_eq!(account_loader.get_account_shared_data(&fee_payer), None);
    }

    // note all magic numbers (how many accounts, how many instructions, how big to size buffers) are arbitrary
    // other than trying not to swamp programs with blank accounts and keep transaction size below the 64mb limit
    #[test]
    fn test_load_transaction_accounts_data_sizes_simd186() {
        let mut rng = rand0_7::thread_rng();
        let mut mock_bank = TestCallbacks::default();

        // arbitrary accounts
        for _ in 0..128 {
            let account = AccountSharedData::create(
                1,
                vec![0; rng.gen_range(0, 128)],
                Pubkey::new_unique(),
                rng.gen(),
                u64::MAX,
            );
            mock_bank.accounts_map.insert(Pubkey::new_unique(), account);
        }

        // fee-payers
        let mut fee_payers = vec![];
        for _ in 0..8 {
            let fee_payer = Pubkey::new_unique();
            let account = AccountSharedData::create(
                LAMPORTS_PER_SOL,
                vec![0; rng.gen_range(0, 32)],
                system_program::id(),
                rng.gen(),
                u64::MAX,
            );
            mock_bank.accounts_map.insert(fee_payer, account);
            fee_payers.push(fee_payer);
        }

        // programs
        let mut loader_owned_accounts = vec![];
        let mut programdata_tracker = AHashMap::new();
        for loader in PROGRAM_OWNERS {
            for _ in 0..16 {
                let program_id = Pubkey::new_unique();
                let mut account = AccountSharedData::create(
                    1,
                    vec![0; rng.gen_range(0, 512)],
                    *loader,
                    rng.gen(),
                    u64::MAX,
                );

                // give half loaderv3 accounts (if they're long enough) a valid programdata
                // a quarter a dead pointer and a quarter nothing
                // we set executable like a program because after the flag is disabled...
                // ...programdata and buffer accounts can be used as program ids without aborting loading
                // this will always fail at execution but we are merely testing the data size accounting here
                if *loader == bpf_loader_upgradeable::id() && account.data().len() >= 64 {
                    let programdata_address = Pubkey::new_unique();
                    let has_programdata = rng.gen();

                    if has_programdata {
                        let programdata_account = AccountSharedData::create(
                            1,
                            vec![0; rng.gen_range(0, 512)],
                            *loader,
                            rng.gen(),
                            u64::MAX,
                        );
                        programdata_tracker.insert(
                            program_id,
                            (programdata_address, programdata_account.data().len()),
                        );
                        mock_bank
                            .accounts_map
                            .insert(programdata_address, programdata_account);
                        loader_owned_accounts.push(programdata_address);
                    }

                    if has_programdata || rng.gen() {
                        account
                            .set_state(&UpgradeableLoaderState::Program {
                                programdata_address,
                            })
                            .unwrap();
                    }
                }

                mock_bank.accounts_map.insert(program_id, account);
                loader_owned_accounts.push(program_id);
            }
        }

        let mut all_accounts = mock_bank.accounts_map.keys().copied().collect::<Vec<_>>();

        // append some to-be-created accounts
        // this is to test that their size is 0 rather than 64
        for _ in 0..32 {
            all_accounts.push(Pubkey::new_unique());
        }

        let mut account_loader = (&mock_bank).into();

        // now generate arbitrary transactions using this accounts
        // we ensure valid fee-payers and that all program ids are loader-owned
        // otherwise any account can appear anywhere
        // some edge cases we hope to hit (not necessarily all in every run):
        // * programs used multiple times as program ids and/or normal accounts are counted once
        // * loaderv3 programdata used explicitly zero one or multiple times is counted once
        // * loaderv3 programs with missing programdata are allowed through
        // * loaderv3 programdata used as program id does nothing weird
        // * loaderv3 programdata used as a regular account does nothing weird
        // * the programdata conditions hold regardless of ordering
        for _ in 0..1024 {
            let mut instructions = vec![];
            for _ in 0..rng.gen_range(1, 8) {
                let mut accounts = vec![];
                for _ in 0..rng.gen_range(1, 16) {
                    all_accounts.shuffle(&mut rng);
                    let pubkey = all_accounts[0];

                    accounts.push(AccountMeta {
                        pubkey,
                        is_writable: rng.gen(),
                        is_signer: rng.gen() && rng.gen(),
                    });
                }

                loader_owned_accounts.shuffle(&mut rng);
                let program_id = loader_owned_accounts[0];
                instructions.push(Instruction {
                    accounts,
                    program_id,
                    data: vec![],
                });
            }

            fee_payers.shuffle(&mut rng);
            let fee_payer = fee_payers[0];
            let fee_payer_account = mock_bank.accounts_map.get(&fee_payer).cloned().unwrap();

            let transaction = SanitizedTransaction::from_transaction_for_tests(
                Transaction::new_with_payer(&instructions, Some(&fee_payer)),
            );

            let mut expected_size = 0;
            let mut counted_programdatas = transaction
                .account_keys()
                .iter()
                .copied()
                .collect::<AHashSet<_>>();

            for pubkey in transaction.account_keys().iter() {
                if let Some(account) = mock_bank.accounts_map.get(pubkey) {
                    expected_size += TRANSACTION_ACCOUNT_BASE_SIZE + account.data().len();
                };

                if let Some((programdata_address, programdata_size)) =
                    programdata_tracker.get(pubkey)
                {
                    if counted_programdatas.get(programdata_address).is_none() {
                        expected_size += TRANSACTION_ACCOUNT_BASE_SIZE + programdata_size;
                        counted_programdatas.insert(*programdata_address);
                    }
                }
            }

            assert!(expected_size <= MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES.get() as usize);

            let loaded_transaction_accounts = load_transaction_accounts(
                &mut account_loader,
                &transaction,
                LoadedTransactionAccount {
                    loaded_size: TRANSACTION_ACCOUNT_BASE_SIZE + fee_payer_account.data().len(),
                    account: fee_payer_account,
                },
                MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
                &mut TransactionErrorMetrics::default(),
                &Rent::default(),
            )
            .unwrap();

            assert_eq!(
                loaded_transaction_accounts.loaded_accounts_data_size,
                expected_size as u32,
            );
        }
    }

    #[test]
    fn test_loader_aliasing() {
        let mut mock_bank = TestCallbacks::default();

        let hit_address = Pubkey::new_unique();
        let miss_address = Pubkey::new_unique();

        let expected_hit_account = AccountSharedData::default();
        mock_bank
            .accounts_map
            .insert(hit_address, expected_hit_account.clone());

        let mut account_loader: AccountLoader<_> = (&mock_bank).into();

        // load hits accounts-db, same account is stored
        account_loader.load_account(&hit_address);
        let actual_hit_account = account_loader.loaded_accounts.get(&hit_address);

        assert_eq!(actual_hit_account, Some(&expected_hit_account));
        assert!(Arc::ptr_eq(
            &actual_hit_account.unwrap().data_clone(),
            &expected_hit_account.data_clone()
        ));

        // reload doesn't affect this
        account_loader.load_account(&hit_address);
        let actual_hit_account = account_loader.loaded_accounts.get(&hit_address);

        assert_eq!(actual_hit_account, Some(&expected_hit_account));
        assert!(Arc::ptr_eq(
            &actual_hit_account.unwrap().data_clone(),
            &expected_hit_account.data_clone()
        ));

        // load misses accounts-db, placeholder is inserted
        account_loader.load_account(&miss_address);
        let expected_miss_account = account_loader
            .loaded_accounts
            .get(&miss_address)
            .unwrap()
            .clone();

        assert!(!Arc::ptr_eq(
            &expected_miss_account.data_clone(),
            &expected_hit_account.data_clone()
        ));

        // reload keeps the same placeholder
        account_loader.load_account(&miss_address);
        let actual_miss_account = account_loader.loaded_accounts.get(&miss_address);

        assert_eq!(actual_miss_account, Some(&expected_miss_account));
        assert!(Arc::ptr_eq(
            &actual_miss_account.unwrap().data_clone(),
            &expected_miss_account.data_clone()
        ));
    }
}
