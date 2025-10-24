//! Solana SVM test harness for instructions.
//!
//! This entrypoint provides an API for Agave's program runtime in order to
//! execute program instructions directly against the VM.

use {
    crate::fixture::{instr_context::InstrContext, instr_effects::InstrEffects},
    agave_precompiles::{get_precompile, is_precompile},
    agave_syscalls::create_program_runtime_environment_v1,
    solana_account::AccountSharedData,
    solana_compute_budget::compute_budget::{ComputeBudget, SVMTransactionExecutionCost},
    solana_hash::Hash,
    solana_instruction::AccountMeta,
    solana_instruction_error::InstructionError,
    solana_precompile_error::PrecompileError,
    solana_program_runtime::{
        invoke_context::{EnvironmentConfig, InvokeContext},
        loaded_programs::{ProgramCacheForTxBatch, ProgramRuntimeEnvironments},
        sysvar_cache::SysvarCache,
    },
    solana_pubkey::Pubkey,
    solana_stable_layout::stable_vec::StableVec,
    solana_svm_callback::{InvokeContextCallback, TransactionProcessingCallback},
    solana_svm_log_collector::LogCollector,
    solana_svm_timings::ExecuteTimings,
    solana_transaction_context::{
        transaction_accounts::KeyedAccountSharedData, IndexOfAccount, InstructionAccount,
        TransactionContext,
    },
    std::{collections::HashSet, sync::Arc},
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

impl TransactionProcessingCallback for InstrContextCallback<'_> {
    fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<(AccountSharedData, u64)> {
        self.0
            .accounts
            .iter()
            .find(|(found_pubkey, _)| *found_pubkey == *pubkey)
            .map(|(_, account)| (AccountSharedData::from(account.clone()), 0u64))
    }
}

fn create_invoke_context_fields(
    input: &mut InstrContext,
) -> Option<(
    TransactionContext,
    SysvarCache,
    ProgramRuntimeEnvironments,
    ProgramCacheForTxBatch,
    Hash,
    u64,
    ComputeBudget,
)> {
    let compute_budget = {
        let mut budget = ComputeBudget::new_with_defaults(false);
        budget.compute_unit_limit = input.cu_avail;
        budget
    };

    let sysvar_cache = crate::sysvar_cache::setup_sysvar_cache(&input.accounts);

    let clock = sysvar_cache.get_clock().unwrap();
    let rent = sysvar_cache.get_rent().unwrap();

    if !input
        .accounts
        .iter()
        .any(|(pubkey, _)| pubkey == &input.instruction.program_id)
    {
        input.accounts.push((
            input.instruction.program_id,
            AccountSharedData::default().into(),
        ));
    }

    let transaction_accounts: Vec<KeyedAccountSharedData> = input
        .accounts
        .iter()
        .map(|(pubkey, account)| (*pubkey, AccountSharedData::from(account.clone())))
        .collect();

    let transaction_context = TransactionContext::new(
        transaction_accounts.clone(),
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

    // Set up the program cache, which will include all builtins by default.
    let mut program_cache =
        crate::program_cache::setup_program_cache(&input.feature_set, clock.slot);

    #[allow(deprecated)]
    let (blockhash, lamports_per_signature) = sysvar_cache
        .get_recent_blockhashes()
        .ok()
        .and_then(|x| (*x).last().cloned())
        .map(|x| (x.blockhash, x.fee_calculator.lamports_per_signature))
        .unwrap_or_default();

    let mut newly_loaded_programs = HashSet::<Pubkey>::new();

    for acc in &input.accounts {
        // FD rejects duplicate account loads
        if !newly_loaded_programs.insert(acc.0) {
            return None;
        }

        if program_cache.find(&acc.0).is_none() {
            // load_program_with_pubkey expects the owner to be one of the bpf loader
            if !solana_sdk_ids::loader_v4::check_id(&acc.1.owner)
                && !solana_sdk_ids::bpf_loader_deprecated::check_id(&acc.1.owner)
                && !solana_sdk_ids::bpf_loader::check_id(&acc.1.owner)
                && !solana_sdk_ids::bpf_loader_upgradeable::check_id(&acc.1.owner)
            {
                continue;
            }
            // https://github.com/anza-xyz/agave/blob/af6930da3a99fd0409d3accd9bbe449d82725bd6/svm/src/program_loader.rs#L124
            /* pub fn load_program_with_pubkey<CB: TransactionProcessingCallback, FG: ForkGraph>(
                callbacks: &CB,
                program_cache: &ProgramCache<FG>,
                pubkey: &Pubkey,
                slot: Slot,
                effective_epoch: Epoch,
                epoch_schedule: &EpochSchedule,
                reload: bool,
            ) -> Option<Arc<ProgramCacheEntry>> { */
            if let Some(loaded_program) = solana_svm::program_loader::load_program_with_pubkey(
                &InstrContextCallback(input),
                &environments,
                &acc.0,
                clock.slot,
                &mut ExecuteTimings::default(),
                false,
            ) {
                program_cache.replenish(acc.0, loaded_program);
            }
        }
    }

    Some((
        transaction_context,
        sysvar_cache,
        environments,
        program_cache,
        blockhash,
        lamports_per_signature,
        compute_budget,
    ))
}

fn get_instr_accounts(
    txn_context: &TransactionContext,
    acct_metas: &StableVec<AccountMeta>,
) -> Vec<InstructionAccount> {
    let mut instruction_accounts: Vec<InstructionAccount> =
        Vec::with_capacity(acct_metas.len().try_into().unwrap());
    for account_meta in acct_metas.iter() {
        let index_in_transaction = txn_context
            .find_index_of_account(&account_meta.pubkey)
            .unwrap_or(txn_context.get_number_of_accounts())
            as IndexOfAccount;
        instruction_accounts.push(InstructionAccount::new(
            index_in_transaction,
            account_meta.is_signer,
            account_meta.is_writable,
        ));
    }
    instruction_accounts
}

/// Execute a single instruction against the Solana VM.
pub fn execute_instr(mut input: InstrContext) -> Option<InstrEffects> {
    let log_collector = LogCollector::new_ref();

    let (
        mut transaction_context,
        sysvar_cache,
        environments,
        mut program_cache,
        blockhash,
        lamports_per_signature,
        compute_budget,
    ) = create_invoke_context_fields(&mut input)?;

    let mut compute_units_consumed = 0u64;
    let runtime_features = input.feature_set.runtime_features();

    let result = {
        let callback = InstrContextCallback(&input);

        let environment_config = EnvironmentConfig::new(
            blockhash,
            lamports_per_signature,
            &callback,
            &runtime_features,
            &environments,
            &environments,
            &sysvar_cache,
        );

        let program_idx =
            transaction_context.find_index_of_account(&input.instruction.program_id)?;

        let instruction_accounts =
            get_instr_accounts(&transaction_context, &input.instruction.accounts);

        let mut invoke_context = InvokeContext::new(
            &mut transaction_context,
            &mut program_cache,
            environment_config,
            Some(log_collector.clone()),
            compute_budget.to_budget(),
            SVMTransactionExecutionCost::default(),
        );

        invoke_context
            .transaction_context
            .configure_next_instruction_for_tests(
                program_idx,
                instruction_accounts,
                input.instruction.data.to_vec(),
            )
            .unwrap();

        if invoke_context.is_precompile(&input.instruction.program_id) {
            let instruction_data = input.instruction.data.iter().copied().collect::<Vec<_>>();
            invoke_context.process_precompile(
                &input.instruction.program_id,
                &input.instruction.data,
                [instruction_data.as_slice()].into_iter(),
            )
        } else {
            invoke_context
                .process_instruction(&mut compute_units_consumed, &mut ExecuteTimings::default())
        }
    };

    let cu_avail = input.cu_avail.saturating_sub(compute_units_consumed);
    let return_data = transaction_context.get_return_data().1.to_vec();

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
    })
}

#[cfg(test)]
mod tests {
    use {
        super::*, agave_feature_set::FeatureSet, solana_account::Account,
        solana_instruction::AccountMeta, solana_pubkey::Pubkey,
        solana_stable_layout::stable_instruction::StableInstruction, solana_sysvar_id::SysvarId,
    };

    #[test]
    fn test_system_program_exec() {
        let system_program_id = solana_sdk_ids::system_program::id();
        let native_loader_id = solana_sdk_ids::native_loader::id();
        let sysvar_id = solana_sysvar_id::id();

        let from_pubkey = Pubkey::new_from_array([1u8; 32]);
        let to_pubkey = Pubkey::new_from_array([2u8; 32]);

        // Create Clock sysvar
        let clock = solana_clock::Clock {
            slot: 10,
            ..Default::default()
        };
        let clock_data = bincode::serialize(&clock).unwrap();

        // Create Rent sysvar
        let rent = solana_rent::Rent::default();
        let rent_data = bincode::serialize(&rent).unwrap();

        // Build the instruction context.
        let context = InstrContext {
            feature_set: FeatureSet::default(),
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
            instruction: StableInstruction {
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
                ]
                .into(),
                data: vec![
                    // Transfer
                    0x02, 0x00, 0x00, 0x00, // Lamports
                    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                ]
                .into(),
            },
            cu_avail: 10000u64,
        };

        // Execute the instruction.
        let effects = execute_instr(context).expect("Instruction execution should succeed");

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
