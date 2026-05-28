//! Shared setup helpers for the execution harnesses.
//!
//! Each helper builds one owned prerequisite for an `InvokeContext` (the
//! transaction context, the runtime environments, the blockhash). The harness
//! still assembles its own `EnvironmentConfig`/`InvokeContext`, since those
//! borrow these pieces — so rather than one big `create_invoke_context_fields`
//! returning a tuple of everything, this is a handful of small, composable
//! pieces the harnesses pick from.

#[cfg(feature = "conformance")]
use solana_account::ReadableAccount;
use {
    solana_account::Account,
    solana_compute_budget::compute_budget::ComputeBudget,
    solana_hash::Hash,
    solana_instruction::Instruction,
    solana_message::SanitizedMessage,
    solana_program_runtime::{
        invoke_context::mock_compile_message,
        loaded_programs::{ProgramRuntimeEnvironment, ProgramRuntimeEnvironments},
        sysvar_cache::SysvarCache,
    },
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_svm_feature_set::SVMFeatureSet,
    solana_svm_transaction::svm_message::SVMStaticMessage,
    solana_syscalls::create_program_runtime_environment,
    solana_transaction_context::transaction::TransactionContext,
};

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
#[cfg(feature = "conformance")]
pub(crate) fn sysvar_cache_from_accounts(accounts: &[(Pubkey, Account)]) -> SysvarCache {
    let mut cache = SysvarCache::default();
    cache.fill_missing_entries(|pubkey, set_sysvar| {
        if let Some(account) = accounts.iter().find(|(key, _)| key == pubkey)
            && account.1.lamports() > 0
        {
            set_sysvar(account.1.data());
        }
    });
    cache
}
