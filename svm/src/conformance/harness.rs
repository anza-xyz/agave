//! Conformance harness.

use {
    super::context::{InstrContext, InstrEffects},
    crate::message_processor::process_message,
    solana_compute_budget::compute_budget::ComputeBudget,
    solana_instruction::error::InstructionError,
    solana_program_runtime::{
        invoke_context::{EnvironmentConfig, InvokeContext, mock_compile_message},
        loaded_programs::{
            ProgramCacheForTxBatch, ProgramRuntimeEnvironment, ProgramRuntimeEnvironments,
        },
        sysvar_cache::SysvarCache,
    },
    solana_pubkey::Pubkey,
    solana_svm_callback::InvokeContextCallback,
    solana_svm_log_collector::LogCollector,
    solana_svm_timings::ExecuteTimings,
    solana_svm_transaction::svm_message::SVMStaticMessage,
    solana_syscalls::create_program_runtime_environment,
    solana_transaction_context::transaction::TransactionContext,
    solana_transaction_error::TransactionError,
    std::rc::Rc,
};

/// Execute a single instruction against the Solana VM with a custom callback.
pub fn execute_instr_with_callback<C: InvokeContextCallback>(
    input: &InstrContext,
    callback: &C,
    compute_budget: &ComputeBudget,
    program_cache: &mut ProgramCacheForTxBatch,
    sysvar_cache: &SysvarCache,
) -> Option<InstrEffects> {
    let mut compute_units_consumed = 0;
    let mut timings = ExecuteTimings::default();

    let log_collector = LogCollector::new_ref();
    let feature_set = input.feature_set;

    let rent = sysvar_cache.get_rent().unwrap();
    let program_id = &input.instruction.program_id;
    let loader_key = program_cache.find(program_id)?.account_owner();

    let (sanitized_message, transaction_accounts) =
        mock_compile_message(&input.instruction, &input.accounts, program_id, &loader_key)?;

    let mut transaction_context = TransactionContext::new(
        transaction_accounts,
        (*rent).clone(),
        compute_budget.max_instruction_stack_depth,
        compute_budget.max_instruction_trace_length,
        sanitized_message.num_instructions(),
    );

    let program_runtime_environment = create_program_runtime_environment(
        &input.feature_set,
        &compute_budget.to_budget(),
        false, /* deployment */
        false, /* debugging_features */
    )
    .unwrap();

    let result = {
        #[expect(deprecated)]
        let (blockhash, blockhash_lamports_per_signature) = sysvar_cache
            .get_recent_blockhashes()
            .ok()
            .and_then(|x| (*x).last().cloned())
            .map(|x| (x.blockhash, x.fee_calculator.lamports_per_signature))
            .unwrap_or_default();

        let program_runtime_environments = ProgramRuntimeEnvironments::new(
            ProgramRuntimeEnvironment::clone(&program_runtime_environment),
            program_runtime_environment,
        );
        let environment_config = EnvironmentConfig::new(
            blockhash,
            blockhash_lamports_per_signature,
            false,
            callback,
            &feature_set,
            &program_runtime_environments,
            sysvar_cache,
        );

        let mut invoke_context = InvokeContext::new(
            &mut transaction_context,
            program_cache,
            environment_config,
            Some(log_collector.clone()),
            compute_budget.to_budget(),
            compute_budget.to_cost(),
        );
        match process_message(
            &sanitized_message,
            &mut invoke_context,
            &mut timings,
            &mut compute_units_consumed,
        ) {
            Ok(()) => Ok(()),
            Err(TransactionError::InstructionError(_, err)) => Err(err),
            // `process_message` only ever returns `InstructionError`-shaped
            // failures.
            Err(_) => unreachable!(),
        }
    };

    let cu_avail = compute_budget
        .compute_unit_limit
        .saturating_sub(compute_units_consumed);
    let return_data = transaction_context.get_return_data().1.to_vec();

    let logs = Rc::try_unwrap(log_collector)
        .ok()
        .map(|cell| cell.into_inner().into_messages())
        .unwrap_or_default();

    let account_keys: Vec<Pubkey> = (0..transaction_context.get_number_of_accounts())
        .map(|index| {
            *transaction_context
                .get_key_of_account_at_index(index)
                .clone()
                .unwrap()
        })
        .collect::<Vec<_>>();

    Some(InstrEffects {
        custom_err: if let Err(InstructionError::Custom(code)) = result {
            Some(code)
        } else {
            None
        },
        result: result.err(),
        resulting_accounts: transaction_context
            .deconstruct_without_keys()
            .unwrap()
            .into_iter()
            .zip(account_keys)
            .map(|(account, key)| (key, account.into()))
            .collect(),
        cu_avail,
        return_data,
        logs,
    })
}
