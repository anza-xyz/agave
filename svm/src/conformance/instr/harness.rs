//! Conformance harness.

use {
    super::{context::InstrContext, effects::InstrEffects},
    crate::conformance::{
        callback::DefaultCallback,
        setup::{
            InvokeContextFields, compute_budget, prepare_invoke_context_fields, program_loader_key,
            program_runtime_environments,
        },
    },
    solana_account::AccountSharedData,
    solana_instruction::error::InstructionError,
    solana_program_runtime::{
        invoke_context::InvokeContext, loaded_programs::ProgramCacheForTxBatch,
        sysvar_cache::SysvarCache,
    },
    solana_pubkey::Pubkey,
    solana_svm_callback::InvokeContextCallback,
    solana_svm_timings::ExecuteTimings,
    std::{collections::HashMap, rc::Rc},
};
#[cfg(feature = "conformance")]
use {
    crate::conformance::{
        callback::ConformanceCallback,
        programs::{fill_program_cache_from_accounts, new_program_cache_with_builtins},
        setup::sysvar_cache_from_accounts,
    },
    agave_precompiles::is_precompile,
    prost::Message,
    protosol::protos::{InstrContext as ProtoInstrContext, InstrEffects as ProtoInstrEffects},
    std::ffi::c_int,
};

/// Execute a single instruction against the Solana VM with the default
/// (no-precompile) callback.
pub fn execute_instr(
    input: &InstrContext,
    program_cache: &mut ProgramCacheForTxBatch,
    sysvar_cache: &SysvarCache,
) -> InstrEffects {
    execute_instr_with_callback(input, &DefaultCallback, program_cache, sysvar_cache)
}

/// Execute a single instruction against the Solana VM with a custom callback.
pub fn execute_instr_with_callback<C: InvokeContextCallback>(
    input: &InstrContext,
    callback: &C,
    program_cache: &mut ProgramCacheForTxBatch,
    sysvar_cache: &SysvarCache,
) -> InstrEffects {
    let mut compute_units_consumed = 0;
    let mut timings = ExecuteTimings::default();

    let mut compute_budget = compute_budget(&input.feature_set);
    compute_budget.compute_unit_limit = input.cu_avail; // Clamp budget for execution by cu_avail

    let loader_key = program_loader_key(&input.accounts, &input.instruction.program_id);

    let program_runtime_environments =
        program_runtime_environments(&input.feature_set, &compute_budget);

    let InvokeContextFields {
        sanitized_message,
        mut transaction_context,
        environment_config,
        log_collector,
        execution_budget,
        execution_cost,
    } = prepare_invoke_context_fields(
        input,
        callback,
        &loader_key,
        sysvar_cache,
        &compute_budget,
        &program_runtime_environments,
    );

    let result = {
        let mut invoke_context = InvokeContext::new(
            &mut transaction_context,
            program_cache,
            environment_config,
            Some(log_collector.clone()),
            execution_budget,
            execution_cost,
        );

        match invoke_context.process_message(
            &sanitized_message,
            &mut timings,
            &mut compute_units_consumed,
        ) {
            Ok(()) => Ok(()),
            Err((_, err)) => Err(err),
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
                .unwrap()
        })
        .collect();

    // Post-execution state of the accounts in the compiled message.
    let mut executed: HashMap<Pubkey, AccountSharedData> = account_keys
        .into_iter()
        .zip(transaction_context.deconstruct_without_keys().unwrap())
        .collect();

    // Preserve input account order, overlaying executed state for accounts
    // present in the compiled message.
    let resulting_accounts = input
        .accounts
        .iter()
        .map(|(pubkey, account)| {
            (
                *pubkey,
                executed
                    .remove(pubkey)
                    .map(Into::into)
                    .unwrap_or_else(|| account.clone()),
            )
        })
        .collect();

    InstrEffects {
        custom_err: if let Err(InstructionError::Custom(code)) = result {
            Some(code)
        } else {
            None
        },
        result: result.err(),
        resulting_accounts,
        cu_avail,
        return_data,
        logs,
    }
}

#[cfg(feature = "conformance")]
pub fn execute_instr_proto(input: ProtoInstrContext) -> ProtoInstrEffects {
    let instr_context = InstrContext::from(input);

    // When testing with protobuf, we fill the sysvar cache from input accounts.
    let sysvar_cache = sysvar_cache_from_accounts(&instr_context.accounts);

    // When testing with protobuf, we fill the program cache from input accounts.
    let mut program_cache = {
        let slot = sysvar_cache.get_clock().unwrap().slot;
        let feature_set = &instr_context.feature_set;
        let compute_budget = compute_budget(feature_set);
        let environments = program_runtime_environments(feature_set, &compute_budget);

        let mut cache = new_program_cache_with_builtins(slot);
        fill_program_cache_from_accounts(
            &mut cache,
            environments.get_env_for_deployment(),
            &instr_context.accounts,
            slot,
        );

        cache
    };

    let mut effects = execute_instr_with_callback(
        &instr_context,
        &ConformanceCallback,
        &mut program_cache,
        &sysvar_cache,
    );

    // Precompile verification failures surface as `Custom`, but Firedancer
    // reports a custom error code of 0 for precompiles.
    if effects.custom_err.is_some()
        && is_precompile(&instr_context.instruction.program_id, |_| true)
    {
        effects.custom_err = Some(0);
    }

    // TODO: Firedancer's tooling compares resulting account contents even
    // when execution fails, so the harness must report them. Account
    // contents are not meaningful on error (partial writes can diverge based
    // on timing, e.g. with direct mapping or builtins), so once the tooling
    // supports it, the harness should skip the account comparison on error
    // entirely, which would also make the CU-exhaustion workaround below
    // unnecessary.
    direct_mapping_handle_cu_exhaustion(
        instr_context.feature_set.virtual_address_space_adjustments,
        effects.cu_avail,
        effects.result.is_some(),
        effects
            .resulting_accounts
            .iter_mut()
            .map(|(_, account)| &mut account.data),
    );

    effects.into()
}

/// Due to how Firedancer's VM CU accounting works, when
/// virtual_address_space_adjustments is enabled and execution fails with the
/// CU meter exhausted, we cannot compare the data region of the accounts with
/// Agave.  Clears each supplied data buffer in that case.
#[cfg(feature = "conformance")]
fn direct_mapping_handle_cu_exhaustion<'a>(
    virtual_address_space_adjustments_active: bool,
    cu_avail: u64,
    has_err: bool,
    account_data: impl IntoIterator<Item = &'a mut Vec<u8>>,
) {
    if virtual_address_space_adjustments_active && cu_avail == 0 && has_err {
        for data in account_data {
            data.clear();
        }
    }
}

/// # Safety
///
/// `in_ptr` must point to `in_sz` initialized bytes. `out_ptr` must point
/// to a writable buffer of at least `*out_psz` bytes. On return, `*out_psz`
/// is updated to the number of bytes written.
#[cfg(feature = "conformance")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sol_compat_instr_execute_v1(
    out_ptr: *mut u8,
    out_psz: *mut u64,
    in_ptr: *mut u8,
    in_sz: u64,
) -> c_int {
    let in_slice = unsafe { std::slice::from_raw_parts(in_ptr, in_sz as usize) };
    let Ok(instr_context) = ProtoInstrContext::decode(in_slice) else {
        return 0;
    };
    let instr_effects = execute_instr_proto(instr_context);
    let out_slice = unsafe { std::slice::from_raw_parts_mut(out_ptr, (*out_psz) as usize) };
    let out_vec = instr_effects.encode_to_vec();
    if out_vec.len() > out_slice.len() {
        return 0;
    }
    out_slice[..out_vec.len()].copy_from_slice(&out_vec);
    unsafe {
        *out_psz = out_vec.len() as u64;
    }
    1
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::conformance::programs::{
            add_program_to_program_cache, keyed_account_for_system_program,
            new_program_cache_with_builtins,
        },
        solana_account::Account,
        solana_instruction::Instruction,
        solana_rent::Rent,
        solana_svm_feature_set::SVMFeatureSet,
        solana_system_program::system_processor::DEFAULT_COMPUTE_UNITS as SYSTEM_TRANSFER_CUS,
        std::cell::RefCell,
        test_case::test_case,
    };

    const NOOP_ELF: &[u8] =
        include_bytes!("../../../../programs/bpf_loader/test_elfs/out/noop_aligned.so");

    const FROM_BASE_LAMPORTS: u64 = 5_000;
    const TO_BASE_LAMPORTS: u64 = 1_000;

    #[derive(Default)]
    struct CountingCallback {
        // Just a simple little mock so we can test our callback is being used.
        precompile_checks: RefCell<u32>,
    }

    impl InvokeContextCallback for CountingCallback {
        fn is_precompile(&self, _program_id: &Pubkey) -> bool {
            *self.precompile_checks.borrow_mut() += 1;
            false
        }
    }

    fn system_account_with_lamports(lamports: u64) -> Account {
        Account {
            lamports,
            data: vec![],
            owner: solana_sdk_ids::system_program::id(),
            executable: false,
            rent_epoch: u64::MAX,
        }
    }

    fn sysvar_cache_with_rent() -> SysvarCache {
        let mut sysvar_cache = SysvarCache::default();
        sysvar_cache.fill_missing_entries(|pubkey, callback| {
            if pubkey == &solana_sdk_ids::sysvar::rent::id() {
                let rent_data = bincode::serialize(&Rent::default()).unwrap();
                callback(&rent_data);
            }
        });
        sysvar_cache
    }

    #[cfg(feature = "conformance")]
    fn proto_account(pubkey: Pubkey, account: Account) -> protosol::protos::AcctState {
        protosol::protos::AcctState {
            address: pubkey.to_bytes().to_vec(),
            owner: account.owner.to_bytes().to_vec(),
            lamports: account.lamports,
            data: account.data,
            executable: account.executable,
        }
    }

    #[cfg(feature = "conformance")]
    fn proto_sysvar_account<T: serde::Serialize>(
        pubkey: Pubkey,
        sysvar: &T,
    ) -> protosol::protos::AcctState {
        protosol::protos::AcctState {
            address: pubkey.to_bytes().to_vec(),
            owner: solana_sdk_ids::sysvar::id().to_bytes().to_vec(),
            lamports: 1,
            data: bincode::serialize(sysvar).unwrap(),
            executable: false,
        }
    }

    fn build_system_transfer_context(from: &Pubkey, to: &Pubkey, amount: u64) -> InstrContext {
        let feature_set = SVMFeatureSet::default();
        let accounts = vec![
            (
                *from,
                system_account_with_lamports(FROM_BASE_LAMPORTS + amount),
            ),
            (*to, system_account_with_lamports(TO_BASE_LAMPORTS)),
            keyed_account_for_system_program(),
        ];
        let instruction = solana_system_interface::instruction::transfer(from, to, amount);
        InstrContext {
            feature_set,
            accounts,
            instruction,
            cu_avail: SYSTEM_TRANSFER_CUS,
        }
    }

    fn assert_system_transfer_effects(
        effects: &InstrEffects,
        from: &Pubkey,
        to: &Pubkey,
        amount: u64,
    ) {
        // Success
        assert_eq!(effects.result, None);
        assert_eq!(effects.custom_err, None);
        // CUs exhausted
        assert_eq!(effects.cu_avail, 0);
        // Lamports transferred
        assert_eq!(
            effects.get_account(from).unwrap().lamports,
            FROM_BASE_LAMPORTS
        );
        assert_eq!(
            effects.get_account(to).unwrap().lamports,
            TO_BASE_LAMPORTS + amount
        );
    }

    #[test]
    fn test_system_program_exec() {
        let from = Pubkey::new_unique();
        let to = Pubkey::new_unique();
        let amount = 1_000;
        let context = build_system_transfer_context(&from, &to, amount);
        let sysvar_cache = sysvar_cache_with_rent();
        let mut program_cache = new_program_cache_with_builtins(0);

        let effects = execute_instr(&context, &mut program_cache, &sysvar_cache);
        assert_system_transfer_effects(&effects, &from, &to, amount);
    }

    #[test]
    fn test_system_program_exec_with_callback() {
        let from = Pubkey::new_unique();
        let to = Pubkey::new_unique();
        let amount = 1_000;
        let context = build_system_transfer_context(&from, &to, amount);
        let sysvar_cache = sysvar_cache_with_rent();
        let mut program_cache = new_program_cache_with_builtins(0);

        let callback = CountingCallback::default();

        let effects =
            execute_instr_with_callback(&context, &callback, &mut program_cache, &sysvar_cache);
        assert_system_transfer_effects(&effects, &from, &to, amount);
    }

    #[test_case(solana_sdk_ids::bpf_loader_deprecated::id(); "loader_v1")]
    #[test_case(solana_sdk_ids::bpf_loader::id(); "loader_v2")]
    #[test_case(solana_sdk_ids::bpf_loader_upgradeable::id(); "loader_v3")]
    fn test_bpf_noop_program_exec(loader_key: Pubkey) {
        let program_id = Pubkey::new_unique();
        let program_account = Account {
            lamports: 1,
            data: vec![],
            owner: loader_key,
            executable: true,
            rent_epoch: u64::MAX,
        };
        let context = InstrContext::new_with_default_budget(
            SVMFeatureSet::default(),
            vec![(program_id, program_account)],
            Instruction::new_with_bytes(program_id, &[], vec![]),
        );
        let sysvar_cache = sysvar_cache_with_rent();

        let mut program_cache = new_program_cache_with_builtins(0);
        add_program_to_program_cache(
            &mut program_cache,
            &program_id,
            &loader_key,
            NOOP_ELF,
            &context.feature_set,
        );

        let effects = execute_instr(&context, &mut program_cache, &sysvar_cache);
        assert_eq!(effects.result, None);
        assert_eq!(effects.custom_err, None);
    }

    #[cfg(feature = "conformance")]
    #[test]
    #[should_panic(expected = "invariant violation: duplicate account load")]
    fn test_duplicate_accounts_panic_with_invariant_violation() {
        let from = Pubkey::new_unique();
        let to = Pubkey::new_unique();
        let duplicate = Pubkey::new_unique();
        let instruction = solana_system_interface::instruction::transfer(&from, &to, 1);

        execute_instr_proto(ProtoInstrContext {
            program_id: solana_sdk_ids::system_program::id().to_bytes().to_vec(),
            accounts: vec![
                proto_account(from, system_account_with_lamports(FROM_BASE_LAMPORTS)),
                proto_account(to, system_account_with_lamports(TO_BASE_LAMPORTS)),
                proto_account(duplicate, system_account_with_lamports(1)),
                proto_account(duplicate, system_account_with_lamports(1)),
                proto_account(
                    keyed_account_for_system_program().0,
                    keyed_account_for_system_program().1,
                ),
                proto_sysvar_account(
                    solana_sdk_ids::sysvar::clock::id(),
                    &solana_clock::Clock::default(),
                ),
            ],
            instr_accounts: vec![
                protosol::protos::InstrAcct {
                    index: 0,
                    is_signer: true,
                    is_writable: true,
                },
                protosol::protos::InstrAcct {
                    index: 1,
                    is_signer: false,
                    is_writable: true,
                },
            ],
            data: instruction.data,
            cu_avail: SYSTEM_TRANSFER_CUS,
            features: None,
        });
    }
}
