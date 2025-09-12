//! Solana SVM fuzz harness for instructions.
//!
//! This entrypoint provides an API for Agave's program runtime in order to
//! execute program instructions directly against the VM.

#![allow(clippy::missing_safety_doc)]

pub mod program_cache;
pub mod sysvar_cache;

use {
    agave_precompiles::{get_precompile, is_precompile},
    prost::Message,
    solana_account::AccountSharedData,
    solana_compute_budget::compute_budget::{ComputeBudget, SVMTransactionExecutionCost},
    solana_hash::Hash,
    solana_instruction::AccountMeta,
    solana_instruction_error::InstructionError,
    solana_precompile_error::PrecompileError,
    solana_program_runtime::{
        invoke_context::{EnvironmentConfig, InvokeContext},
        loaded_programs::ProgramCacheForTxBatch,
        sysvar_cache::SysvarCache,
    },
    solana_pubkey::Pubkey,
    solana_stable_layout::stable_vec::StableVec,
    solana_svm_callback::{InvokeContextCallback, TransactionProcessingCallback},
    solana_svm_fuzz_harness_fixture::{
        instr_context::InstrContext,
        instr_effects::InstrEffects,
        proto::{InstrContext as ProtoInstrContext, InstrEffects as ProtoInstrEffects},
    },
    solana_svm_log_collector::LogCollector,
    solana_svm_timings::ExecuteTimings,
    solana_transaction_context::{
        transaction_accounts::TransactionAccount, IndexOfAccount, InstructionAccount,
        TransactionContext,
    },
    std::{collections::HashSet, ffi::c_int},
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

    // Add checks for rent boundaries
    let rent_ = sysvar_cache.get_rent().unwrap();
    let rent = (*rent_).clone();
    if rent.lamports_per_byte_year > u32::MAX.into()
        || rent.exemption_threshold > 999.0
        || rent.exemption_threshold < 0.0
        || rent.burn_percent > 100
    {
        return None;
    };

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

    let transaction_accounts: Vec<TransactionAccount> = input
        .accounts
        .iter()
        .map(|(pubkey, account)| (*pubkey, AccountSharedData::from(account.clone())))
        .collect();

    let transaction_context = TransactionContext::new(
        transaction_accounts.clone(),
        rent,
        compute_budget.max_instruction_stack_depth,
        compute_budget.max_instruction_trace_length,
    );

    // Set up the program cache, which will include all builtins by default.
    let mut program_cache =
        crate::program_cache::setup_program_cache(&input.feature_set, &compute_budget, clock.slot);

    let environments = program_cache.environments.clone();

    #[allow(deprecated)]
    let (blockhash, lamports_per_signature) = sysvar_cache
        .get_recent_blockhashes()
        .ok()
        .and_then(|x| (*x).last().cloned())
        .map(|x| (x.blockhash, x.fee_calculator.lamports_per_signature))
        .unwrap_or_default();

    input.last_blockhash = blockhash;
    input.lamports_per_signature = lamports_per_signature;

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

fn execute_instr(mut input: InstrContext) -> Option<InstrEffects> {
    let log_collector = LogCollector::new_ref();

    let (
        mut transaction_context,
        sysvar_cache,
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
                &input.instruction.data,
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

pub fn execute_instr_proto(input: ProtoInstrContext) -> Option<ProtoInstrEffects> {
    let Ok(instr_context) = InstrContext::try_from(input) else {
        return None;
    };
    let instr_effects = execute_instr(instr_context);
    instr_effects.map(Into::into)
}

#[no_mangle]
pub unsafe extern "C" fn sol_compat_instr_execute_v1(
    out_ptr: *mut u8,
    out_psz: *mut u64,
    in_ptr: *mut u8,
    in_sz: u64,
) -> c_int {
    let in_slice = std::slice::from_raw_parts(in_ptr, in_sz as usize);
    let Ok(instr_context) = ProtoInstrContext::decode(in_slice) else {
        return 0;
    };
    let Some(instr_effects) = execute_instr_proto(instr_context) else {
        return 0;
    };
    let out_slice = std::slice::from_raw_parts_mut(out_ptr, (*out_psz) as usize);
    let out_vec = instr_effects.encode_to_vec();
    if out_vec.len() > out_slice.len() {
        return 0;
    }
    out_slice[..out_vec.len()].copy_from_slice(&out_vec);
    *out_psz = out_vec.len() as u64;

    1
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_svm_fuzz_harness_fixture::proto::{
            AcctState as ProtoAcctState, InstrAcct as ProtoInstrAcct,
        },
    };

    #[test]
    fn test_system_program_exec() {
        let native_loader_id = solana_sdk_ids::native_loader::id().to_bytes().to_vec();

        // Ensure that a basic account transfer works
        let input = ProtoInstrContext {
            program_id: vec![0u8; 32],
            accounts: vec![
                ProtoAcctState {
                    address: vec![1u8; 32],
                    owner: vec![0u8; 32],
                    lamports: 1000,
                    data: vec![],
                    executable: false,
                    seed_addr: None,
                },
                ProtoAcctState {
                    address: vec![2u8; 32],
                    owner: vec![0u8; 32],
                    lamports: 0,
                    data: vec![],
                    executable: false,
                    seed_addr: None,
                },
                ProtoAcctState {
                    address: vec![0u8; 32],
                    owner: native_loader_id.clone(),
                    lamports: 10000000,
                    data: b"Solana Program".to_vec(),
                    executable: true,
                    seed_addr: None,
                },
            ],
            instr_accounts: vec![
                ProtoInstrAcct {
                    index: 0,
                    is_signer: true,
                    is_writable: true,
                },
                ProtoInstrAcct {
                    index: 1,
                    is_signer: false,
                    is_writable: true,
                },
            ],
            data: vec![
                // Transfer
                0x02, 0x00, 0x00, 0x00, // Lamports
                0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            ],
            cu_avail: 10000u64,
            epoch_context: None,
            slot_context: None,
        };
        let output = execute_instr_proto(input);
        assert_eq!(
            output,
            Some(ProtoInstrEffects {
                result: 0,
                custom_err: 0,
                modified_accounts: vec![
                    ProtoAcctState {
                        address: vec![1u8; 32],
                        owner: vec![0u8; 32],
                        lamports: 999,
                        data: vec![],
                        executable: false,
                        seed_addr: None,
                    },
                    ProtoAcctState {
                        address: vec![2u8; 32],
                        owner: vec![0u8; 32],
                        lamports: 1,
                        data: vec![],
                        executable: false,
                        seed_addr: None,
                    },
                    ProtoAcctState {
                        address: vec![0u8; 32],
                        owner: native_loader_id.clone(),
                        lamports: 10000000,
                        data: b"Solana Program".to_vec(),
                        executable: true,
                        seed_addr: None,
                    },
                ],
                cu_avail: 9850u64,
                return_data: vec![],
            })
        );
    }
}
