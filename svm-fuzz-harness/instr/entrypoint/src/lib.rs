//! Solana SVM fuzz harness instruction entrypoint.
//!
//! This entrypoint provides an API for Agave's program runtime in order to
//! execute program instructions directly against the VM.
//!
//! It is primarily used by the fuzz harness, located in this same parent
//! directory, however it can also be used standalone for other testing tools,
//! such as those which test BPF programs.

use {
    solana_account::AccountSharedData,
    solana_compute_budget::compute_budget::ComputeBudget,
    solana_feature_set::FeatureSet,
    solana_hash::Hash,
    solana_instruction::error::InstructionError,
    solana_program_runtime::{
        invoke_context::{EnvironmentConfig, InvokeContext},
        loaded_programs::ProgramCacheForTxBatch,
        sysvar_cache::SysvarCache,
    },
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_timings::ExecuteTimings,
    solana_transaction_context::{InstructionAccount, TransactionContext},
    std::sync::Arc,
};

/// The overall result of the instruction.
#[derive(Debug, PartialEq, Eq)]
pub struct InstructionResult {
    /// The number of compute units consumed by the instruction.
    pub compute_units_consumed: u64,
    /// The time taken to execute the instruction.
    pub execution_time: u64,
    /// The result code of the program's execution.
    pub program_result: Result<(), InstructionError>,
    /// The return data produced by the instruction, if any.
    pub return_data: Vec<u8>,
    /// The resulting accounts after executing the instruction.
    ///
    /// This includes all accounts provided to the processor, in the order
    /// they were provided. Any accounts that were modified will maintain
    /// their original position in this list, but with updated state.
    pub resulting_accounts: Vec<(Pubkey, AccountSharedData)>,
}

/// Contextual values for the execution environment.
pub struct EnvironmentContext<'a> {
    blockhash: &'a Hash,
    compute_budget: &'a ComputeBudget,
    feature_set: &'a FeatureSet,
    lamports_per_signature: u64,
    rent: &'a Rent,
    sysvar_cache: &'a SysvarCache,
}

impl<'a> EnvironmentContext<'a> {
    pub fn new(
        blockhash: &'a Hash,
        compute_budget: &'a ComputeBudget,
        feature_set: &'a FeatureSet,
        lamports_per_signature: u64,
        rent: &'a Rent,
        sysvar_cache: &'a SysvarCache,
    ) -> Self {
        Self {
            blockhash,
            compute_budget,
            feature_set,
            lamports_per_signature,
            rent,
            sysvar_cache,
        }
    }
}

pub fn process_instruction(
    program_id: &Pubkey,
    instruction_accounts: &[InstructionAccount],
    instruction_data: &[u8],
    transaction_accounts: &[(Pubkey, AccountSharedData)],
    program_cache: &mut ProgramCacheForTxBatch,
    environment_context: EnvironmentContext,
) -> InstructionResult {
    let mut compute_units_consumed = 0;
    let mut timings = ExecuteTimings::default();

    let program_id_index = transaction_accounts
        .iter()
        .position(|(pubkey, _)| pubkey == program_id)
        .expect("Program account not found in transaction accounts")
        as u16;

    let mut transaction_context = TransactionContext::new(
        transaction_accounts.to_vec(),
        environment_context.rent.clone(),
        environment_context
            .compute_budget
            .max_instruction_stack_depth,
        environment_context
            .compute_budget
            .max_instruction_trace_length,
    );

    let program_result = {
        let mut invoke_context = InvokeContext::new(
            &mut transaction_context,
            program_cache,
            EnvironmentConfig::new(
                *environment_context.blockhash,
                environment_context.lamports_per_signature,
                0,
                &|_| 0,
                Arc::new(environment_context.feature_set.clone()),
                environment_context.sysvar_cache,
            ),
            None,
            *environment_context.compute_budget,
        );

        if let Some(precompile) = solana_precompiles::get_precompile(program_id, |feature_id| {
            invoke_context.get_feature_set().is_active(feature_id)
        }) {
            invoke_context.process_precompile(
                precompile,
                instruction_data,
                instruction_accounts,
                &[program_id_index],
                std::iter::once(instruction_data),
            )
        } else {
            invoke_context.process_instruction(
                instruction_data,
                instruction_accounts,
                &[program_id_index],
                &mut compute_units_consumed,
                &mut timings,
            )
        }
    };

    let return_data = transaction_context.get_return_data().1.to_vec();

    let resulting_accounts: Vec<(Pubkey, AccountSharedData)> = transaction_accounts
        .iter()
        .map(|(pubkey, account)| {
            transaction_context
                .find_index_of_account(pubkey)
                .map(|index| {
                    let resulting_account =
                        transaction_context.get_account_at_index(index).unwrap();
                    (*pubkey, resulting_account.take())
                })
                .unwrap_or((*pubkey, account.clone()))
        })
        .collect();

    InstructionResult {
        compute_units_consumed,
        execution_time: timings.details.execute_us.0,
        program_result,
        return_data,
        resulting_accounts,
    }
}
