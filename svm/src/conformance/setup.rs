//! Shared setup helpers for the execution harnesses.

#[cfg(any(feature = "conformance", feature = "dev-context-only-utils"))]
use solana_account::ReadableAccount;
#[cfg(any(feature = "conformance", test))]
use {
    crate::conformance::transaction_address_loader::TransactionAddressLoader,
    solana_message::{SanitizedVersionedMessage, VersionedMessage},
    solana_slot_hashes::SlotHashes,
    std::collections::HashSet,
};
use {
    crate::conformance::{instr::context::InstrContext, nonce_fields::NonceFields},
    solana_account::Account,
    solana_compute_budget::compute_budget::ComputeBudget,
    solana_hash::Hash,
    solana_instruction::Instruction,
    solana_message::SanitizedMessage,
    solana_program_runtime::{
        execution_budget::{SVMTransactionExecutionBudget, SVMTransactionExecutionCost},
        invoke_context::{EnvironmentConfig, mock_compile_message},
        loaded_programs::{ProgramRuntimeEnvironment, ProgramRuntimeEnvironments},
        sysvar_cache::SysvarCache,
    },
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_svm_callback::InvokeContextCallback,
    solana_svm_feature_set::SVMFeatureSet,
    solana_svm_log_collector::LogCollector,
    solana_svm_transaction::svm_message::SVMStaticMessage,
    solana_syscalls::create_program_runtime_environment,
    solana_transaction_context::transaction::TransactionContext,
    std::{cell::RefCell, rc::Rc},
};

/// Fields required by `InvokeContext::new`.
pub(crate) struct InvokeContextFields<'a, 'ix_data> {
    pub(crate) sanitized_message: SanitizedMessage,
    pub(crate) transaction_context: TransactionContext<'ix_data>,
    pub(crate) environment_config: EnvironmentConfig<'a>,
    pub(crate) log_collector: Rc<RefCell<LogCollector>>,
    pub(crate) execution_budget: SVMTransactionExecutionBudget,
    pub(crate) execution_cost: SVMTransactionExecutionCost,
}

/// Compile a sanitized transaction message then instantiate a transaction
/// context as well as the remaining fields required by `InvokeContext::new`.
pub(crate) fn prepare_invoke_context_fields<'a, C: InvokeContextCallback>(
    instr_context: &'a InstrContext,
    callback: &'a C,
    loader_key: &Pubkey,
    sysvar_cache: &'a SysvarCache,
    compute_budget: &ComputeBudget,
    program_runtime_environments: &'a ProgramRuntimeEnvironments,
) -> InvokeContextFields<'a, 'a> {
    let rent = sysvar_cache.get_rent().unwrap();

    let (sanitized_message, transaction_context) = compile_transaction_context(
        &instr_context.instruction,
        &instr_context.accounts,
        &instr_context.instruction.program_id,
        loader_key,
        compute_budget,
        (*rent).clone(),
    );

    let (blockhash, blockhash_lamports_per_signature) = recent_blockhash(sysvar_cache);
    let environment_config = EnvironmentConfig::new(
        blockhash,
        blockhash_lamports_per_signature,
        false,
        callback,
        &instr_context.feature_set,
        program_runtime_environments,
        sysvar_cache,
    );

    let log_collector = LogCollector::new_ref();
    let execution_budget = compute_budget.to_budget();
    let execution_cost = compute_budget.to_cost();

    InvokeContextFields {
        sanitized_message,
        transaction_context,
        environment_config,
        log_collector,
        execution_budget,
        execution_cost,
    }
}

/// Instantiate the fields required by `InvokeContext::new` for an already
/// sanitized transaction message and transaction context.
pub(crate) fn prepare_transaction_invoke_context_fields<'a, 'b, C: InvokeContextCallback>(
    sanitized_message: SanitizedMessage,
    transaction_context: TransactionContext<'b>,
    callback: &'a C,
    feature_set: &'a SVMFeatureSet,
    sysvar_cache: &'a SysvarCache,
    compute_budget: &ComputeBudget,
    execution_budget: SVMTransactionExecutionBudget,
    program_runtime_environments: &'a ProgramRuntimeEnvironments,
    nonce_fields: NonceFields,
) -> InvokeContextFields<'a, 'b> {
    let environment_config = EnvironmentConfig::new(
        nonce_fields.blockhash,
        nonce_fields.blockhash_lamports_per_signature,
        false,
        callback,
        feature_set,
        program_runtime_environments,
        sysvar_cache,
    );

    let log_collector = LogCollector::new_ref();
    let execution_cost = compute_budget.to_cost();

    InvokeContextFields {
        sanitized_message,
        transaction_context,
        environment_config,
        log_collector,
        execution_budget,
        execution_cost,
    }
}

// Create a compute budget from the given feature set.
pub(crate) fn compute_budget(feature_set: &SVMFeatureSet) -> ComputeBudget {
    let simd_0268_active = feature_set.raise_cpi_nesting_limit_to_8;
    ComputeBudget::new_with_defaults(simd_0268_active)
}

/// The loader that owns the program account in `accounts`, used as the program
/// account's owner when compiling the transaction. `None` if the program
/// account isn't present.
pub(crate) fn program_loader_key(accounts: &[(Pubkey, Account)], program_id: &Pubkey) -> Pubkey {
    accounts
        .iter()
        .find(|(key, _)| key == program_id)
        .map(|(_, account)| account.owner)
        .expect("program not found in accounts")
}

/// Compile `instruction` into a sanitized message and a fresh transaction
/// context sized for a single top-level instruction.
pub(crate) fn compile_transaction_context(
    instruction: &Instruction,
    accounts: &[(Pubkey, Account)],
    program_id: &Pubkey,
    loader_key: &Pubkey,
    compute_budget: &ComputeBudget,
    rent: Rent,
) -> (SanitizedMessage, TransactionContext<'static>) {
    let (sanitized_message, transaction_accounts) =
        mock_compile_message(instruction, accounts, program_id, loader_key);
    let transaction_context = TransactionContext::new(
        transaction_accounts,
        rent,
        compute_budget.max_instruction_stack_depth,
        compute_budget.max_instruction_trace_length,
        sanitized_message.num_instructions(),
    );
    (sanitized_message, transaction_context)
}

/// Sanitize a versioned message, resolving address table lookups from the
/// supplied accounts and their sysvar state.
#[cfg(any(feature = "conformance", test))]
pub fn sanitized_message_from_versioned_message(
    message: VersionedMessage,
    accounts: &[(Pubkey, Account)],
) -> SanitizedMessage {
    let sysvar_cache = sysvar_cache_from_accounts(accounts);
    let slot = sysvar_cache
        .get_clock()
        .map(|clock| clock.slot)
        .unwrap_or_default();
    let slot_hashes = sysvar_cache
        .get_slot_hashes()
        .map(|slot_hashes| SlotHashes::new(slot_hashes.slot_hashes()))
        .unwrap_or_else(|_| SlotHashes::new(&[]));
    let message = SanitizedVersionedMessage::try_new(message)
        .expect("transaction context message must be sanitized");

    SanitizedMessage::try_new(
        message,
        TransactionAddressLoader {
            accounts,
            slot,
            slot_hashes: &slot_hashes,
        },
        &HashSet::new(),
    )
    .expect("transaction context message must resolve address table lookups")
}

/// The paired (execution + deployment) program runtime environments for a
/// harness invocation. Both halves share one environment.
pub(crate) fn program_runtime_environments(
    feature_set: &SVMFeatureSet,
    compute_budget: &ComputeBudget,
) -> ProgramRuntimeEnvironments {
    let environment = create_program_runtime_environment(
        feature_set,
        &compute_budget.to_budget(),
        false, /* deployment */
        false, /* debugging_features */
    )
    .unwrap();
    ProgramRuntimeEnvironments::new(ProgramRuntimeEnvironment::clone(&environment), environment)
}

/// The most recent blockhash and its lamports-per-signature from the sysvar
/// cache, or defaults when unavailable.
pub(crate) fn recent_blockhash(sysvar_cache: &SysvarCache) -> (Hash, u64) {
    #[expect(deprecated)]
    sysvar_cache
        .get_recent_blockhashes()
        .ok()
        .and_then(|entries| entries.last().cloned())
        .map(|entry| (entry.blockhash, entry.fee_calculator.lamports_per_signature))
        .unwrap_or_default()
}

/// Build a sysvar cache populated from any sysvar accounts present in the
/// input account set.
#[cfg(any(feature = "conformance", test))]
pub(crate) fn sysvar_cache_from_accounts(accounts: &[(Pubkey, Account)]) -> SysvarCache {
    let mut cache = SysvarCache::default();
    cache.fill_missing_entries(|pubkey, set_sysvar| {
        if let Some(data) = sysvar_account_data(accounts, pubkey) {
            set_sysvar(data);
        }
    });
    cache
}

/// Read and bincode-decode a sysvar account from the input set, ignoring
/// zero-lamport (nonexistent) entries.
#[cfg(any(feature = "conformance", feature = "dev-context-only-utils"))]
pub fn sysvar_from_accounts<T, A>(accounts: &[(Pubkey, A)], id: &Pubkey) -> T
where
    T: serde::de::DeserializeOwned,
    A: ReadableAccount,
{
    bincode::deserialize(sysvar_account_data(accounts, id).unwrap()).unwrap()
}

#[cfg(any(feature = "conformance", feature = "dev-context-only-utils"))]
fn sysvar_account_data<'a, A>(accounts: &'a [(Pubkey, A)], id: &Pubkey) -> Option<&'a [u8]>
where
    A: ReadableAccount,
{
    accounts
        .iter()
        .find(|(address, account)| address == id && account.lamports() > 0)
        .map(|(_, account)| account.data())
}
