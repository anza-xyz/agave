//! Instruction harness.

use {
    crate::fixture::{instr_context::InstrContext, instr_effects::InstrEffects},
    agave_syscalls::create_program_runtime_environment,
    solana_compute_budget::compute_budget::ComputeBudget,
    solana_instruction_error::InstructionError,
    solana_program_runtime::{
        invoke_context::{EnvironmentConfig, InvokeContext, mock_compile_message},
        loaded_programs::ProgramCacheForTxBatch,
        sysvar_cache::SysvarCache,
    },
    solana_pubkey::Pubkey,
    solana_svm_callback::InvokeContextCallback,
    solana_svm_log_collector::LogCollector,
    solana_svm_timings::ExecuteTimings,
    solana_svm_transaction::svm_message::SVMStaticMessage,
    solana_transaction_context::transaction::TransactionContext,
    std::rc::Rc,
};

/// Default callback with no precompile support.
struct DefaultCallback;

impl InvokeContextCallback for DefaultCallback {}

/// Execute a single instruction against the Solana VM.
///
/// This version does not support precompiles. Use [`execute_instr_with_callback`]
/// if you need precompile support.
pub fn execute_instr(
    input: InstrContext,
    compute_budget: &ComputeBudget,
    program_cache: &mut ProgramCacheForTxBatch,
    sysvar_cache: &SysvarCache,
) -> Option<InstrEffects> {
    execute_instr_with_callback(
        &input,
        &DefaultCallback,
        compute_budget,
        program_cache,
        sysvar_cache,
    )
}

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

        let mut invoke_context = InvokeContext::new(
            &mut transaction_context,
            program_cache,
            EnvironmentConfig::new(
                blockhash,
                blockhash_lamports_per_signature,
                callback,
                &feature_set,
                &program_runtime_environment,
                &program_runtime_environment,
                sysvar_cache,
            ),
            Some(log_collector.clone()),
            compute_budget.to_budget(),
            compute_budget.to_cost(),
        );

        invoke_context
            .prepare_top_level_instructions(&sanitized_message)
            .ok()?;

        if invoke_context.is_precompile(&input.instruction.program_id) {
            invoke_context.process_precompile(
                &input.instruction.program_id,
                &input.instruction.data,
                [input.instruction.data.as_slice()].into_iter(),
            )
        } else {
            invoke_context.process_instruction(&mut compute_units_consumed, &mut timings)
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

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_account::Account,
        solana_instruction::{AccountMeta, Instruction},
        solana_pubkey::Pubkey,
        solana_svm_feature_set::SVMFeatureSet,
        solana_sysvar_id::SysvarId,
    };

    #[test]
    fn test_system_program_exec() {
        let system_program_id = solana_sdk_ids::system_program::id();
        let native_loader_id = solana_sdk_ids::native_loader::id();
        let sysvar_id = solana_sysvar_id::id();

        let from_pubkey = Pubkey::new_from_array([1u8; 32]);
        let to_pubkey = Pubkey::new_from_array([2u8; 32]);

        let cu_avail = 10000u64;
        let slot = 10;
        let feature_set = SVMFeatureSet::default();

        // Create Clock sysvar
        let clock = solana_clock::Clock {
            slot,
            ..Default::default()
        };
        let clock_data = bincode::serialize(&clock).unwrap();

        // Create Rent sysvar
        let rent = solana_rent::Rent::default();
        let rent_data = bincode::serialize(&rent).unwrap();

        // Build the instruction context.
        let context = InstrContext {
            feature_set,
            accounts: vec![
                (
                    from_pubkey,
                    Account {
                        lamports: 1000,
                        data: vec![],
                        owner: system_program_id,
                        executable: false,
                        rent_epoch: u64::MAX,
                    },
                ),
                (
                    to_pubkey,
                    Account {
                        lamports: 0,
                        data: vec![],
                        owner: system_program_id,
                        executable: false,
                        rent_epoch: u64::MAX,
                    },
                ),
                (
                    system_program_id,
                    Account {
                        lamports: 10000000,
                        data: b"Solana Program".to_vec(),
                        owner: native_loader_id,
                        executable: true,
                        rent_epoch: u64::MAX,
                    },
                ),
                (
                    solana_clock::Clock::id(),
                    Account {
                        lamports: 1,
                        data: clock_data,
                        owner: sysvar_id,
                        executable: false,
                        rent_epoch: u64::MAX,
                    },
                ),
                (
                    solana_rent::Rent::id(),
                    Account {
                        lamports: 1,
                        data: rent_data,
                        owner: sysvar_id,
                        executable: false,
                        rent_epoch: u64::MAX,
                    },
                ),
            ],
            instruction: Instruction {
                program_id: system_program_id,
                accounts: vec![
                    AccountMeta {
                        pubkey: from_pubkey,
                        is_signer: true,
                        is_writable: true,
                    },
                    AccountMeta {
                        pubkey: to_pubkey,
                        is_signer: false,
                        is_writable: true,
                    },
                ],
                data: vec![
                    // Transfer
                    0x02, 0x00, 0x00, 0x00, // Lamports
                    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                ],
            },
        };

        // Set up the Compute Budget.
        let compute_budget = {
            let mut budget = ComputeBudget::new_with_defaults(false);
            budget.compute_unit_limit = cu_avail;
            budget
        };

        // Create Sysvar Cache
        let mut sysvar_cache = SysvarCache::default();
        crate::sysvar_cache::fill_from_accounts(&mut sysvar_cache, &context.accounts);

        // Create Program Cache
        let mut program_cache = crate::program_cache::new_with_builtins(slot);

        let environments = create_program_runtime_environment(
            &context.feature_set,
            &compute_budget.to_budget(),
            false, /* deployment */
            false, /* debugging_features */
        )
        .unwrap();

        crate::program_cache::fill_from_accounts(
            &mut program_cache,
            &environments,
            &context.accounts,
            slot,
        )
        .unwrap();

        // Execute the instruction.
        let effects = execute_instr(context, &compute_budget, &mut program_cache, &sysvar_cache)
            .expect("Instruction execution should succeed");

        // Verify the results.
        assert_eq!(effects.result, None);
        assert_eq!(effects.custom_err, None);
        assert_eq!(effects.cu_avail, 9850u64);
        assert_eq!(effects.return_data, Vec::<u8>::new(),);

        // Verify account changes.
        let from_account = effects
            .resulting_accounts
            .iter()
            .find(|(k, _)| k == &from_pubkey)
            .unwrap();
        assert_eq!(from_account.1.lamports, 999);

        let to_account = effects
            .resulting_accounts
            .iter()
            .find(|(k, _)| k == &to_pubkey)
            .unwrap();
        assert_eq!(to_account.1.lamports, 1);
    }
}
