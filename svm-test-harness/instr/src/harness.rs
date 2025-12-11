//! Instruction harness.

use {
    crate::{
        compile_accounts::{compile_accounts, CompiledAccounts},
        fixture::{instr_context::InstrContext, instr_effects::InstrEffects},
    },
    agave_precompiles::{get_precompile, is_precompile},
    agave_syscalls::create_program_runtime_environment_v1,
    solana_account::Account,
    solana_compute_budget::compute_budget::ComputeBudget,
    solana_instruction_error::InstructionError,
    solana_precompile_error::PrecompileError,
    solana_program_runtime::{
        invoke_context::{EnvironmentConfig, InvokeContext},
        loaded_programs::{ProgramCacheForTxBatch, ProgramRuntimeEnvironments},
        sysvar_cache::SysvarCache,
    },
    solana_pubkey::Pubkey,
    solana_svm_callback::InvokeContextCallback,
    solana_svm_log_collector::LogCollector,
    solana_svm_timings::ExecuteTimings,
    solana_transaction_context::TransactionContext,
    std::{collections::HashMap, rc::Rc, sync::Arc},
};

/// Implement the callback trait so that the SVM API can be used to load
/// program ELFs from accounts (ie. `load_program_with_pubkey`).
struct InstrContextCallback<'a>(&'a InstrContext);

impl InvokeContextCallback for InstrContextCallback<'_> {
    fn is_precompile(&self, program_id: &Pubkey) -> bool {
        is_precompile(program_id, |feature_id: &Pubkey| {
            self.0.feature_set.is_active(feature_id)
        })
    }

    fn process_precompile(
        &self,
        program_id: &Pubkey,
        data: &[u8],
        instruction_datas: Vec<&[u8]>,
    ) -> std::result::Result<(), PrecompileError> {
        if let Some(precompile) = get_precompile(program_id, |feature_id: &Pubkey| {
            self.0.feature_set.is_active(feature_id)
        }) {
            precompile.verify(data, &instruction_datas, &self.0.feature_set)
        } else {
            Err(PrecompileError::InvalidPublicKey)
        }
    }
}

/// Execute a single instruction against the Solana VM.
pub fn execute_instr(
    input: InstrContext,
    compute_budget: &ComputeBudget,
    program_cache: &mut ProgramCacheForTxBatch,
    sysvar_cache: &SysvarCache,
) -> Option<InstrEffects> {
    let mut compute_units_consumed = 0;
    let mut timings = ExecuteTimings::default();

    let log_collector = LogCollector::new_ref();
    let runtime_features = input.feature_set.runtime_features();

    let rent = sysvar_cache.get_rent().unwrap();

    let program_id = &input.instruction.program_id;
    let loader_key = program_cache.find(program_id)?.account_owner();

    // Stub the program account if it wasn't provided.
    let mut fallbacks = HashMap::new();
    if !input.accounts.iter().any(|(key, _)| key == program_id) {
        fallbacks.insert(
            *program_id,
            Account {
                owner: loader_key,
                executable: true,
                ..Default::default()
            },
        );
    }

    let CompiledAccounts {
        program_id_index,
        instruction_accounts,
        transaction_accounts,
    } = compile_accounts(&input.instruction, input.accounts.iter(), &fallbacks);

    let mut transaction_context = TransactionContext::new(
        transaction_accounts,
        (*rent).clone(),
        compute_budget.max_instruction_stack_depth,
        compute_budget.max_instruction_trace_length,
    );

    let environments = ProgramRuntimeEnvironments {
        program_runtime_v1: Arc::new(
            create_program_runtime_environment_v1(
                &input.feature_set.runtime_features(),
                &compute_budget.to_budget(),
                false, /* deployment */
                false, /* debugging_features */
            )
            .unwrap(),
        ),
        ..ProgramRuntimeEnvironments::default()
    };

    let result = {
        #[allow(deprecated)]
        let (blockhash, blockhash_lamports_per_signature) = sysvar_cache
            .get_recent_blockhashes()
            .ok()
            .and_then(|x| (*x).last().cloned())
            .map(|x| (x.blockhash, x.fee_calculator.lamports_per_signature))
            .unwrap_or_default();

        let callback = InstrContextCallback(&input);

        let mut invoke_context = InvokeContext::new(
            &mut transaction_context,
            program_cache,
            EnvironmentConfig::new(
                blockhash,
                blockhash_lamports_per_signature,
                &callback,
                &runtime_features,
                &environments,
                &environments,
                sysvar_cache,
            ),
            Some(log_collector.clone()),
            compute_budget.to_budget(),
            compute_budget.to_cost(),
        );

        invoke_context
            .transaction_context
            .configure_next_instruction_for_tests(
                program_id_index,
                instruction_accounts,
                input.instruction.data.to_vec(),
            )
            .unwrap();

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
            if get_precompile(&input.instruction.program_id, |_| true).is_some() {
                Some(0)
            } else {
                Some(code)
            }
        } else {
            None
        },
        result: result.err(),
        modified_accounts: transaction_context
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
        agave_feature_set::FeatureSet,
        solana_account::Account,
        solana_instruction::{AccountMeta, Instruction},
        solana_pubkey::Pubkey,
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
        let feature_set = FeatureSet::default();

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
            feature_set: feature_set.clone(),
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
            let mut budget = ComputeBudget::new_with_defaults(false, false);
            budget.compute_unit_limit = cu_avail;
            budget
        };

        // Create Sysvar Cache
        let mut sysvar_cache = SysvarCache::default();
        crate::sysvar_cache::fill_from_accounts(&mut sysvar_cache, &context.accounts);

        // Create Program Cache
        let mut program_cache = crate::program_cache::new_with_builtins(&feature_set, slot);

        let environments = ProgramRuntimeEnvironments {
            program_runtime_v1: Arc::new(
                create_program_runtime_environment_v1(
                    &feature_set.runtime_features(),
                    &compute_budget.to_budget(),
                    false, /* deployment */
                    false, /* debugging_features */
                )
                .unwrap(),
            ),
            ..ProgramRuntimeEnvironments::default()
        };

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
            .modified_accounts
            .iter()
            .find(|(k, _)| k == &from_pubkey)
            .unwrap();
        assert_eq!(from_account.1.lamports, 999);

        let to_account = effects
            .modified_accounts
            .iter()
            .find(|(k, _)| k == &to_pubkey)
            .unwrap();
        assert_eq!(to_account.1.lamports, 1);
    }
}
