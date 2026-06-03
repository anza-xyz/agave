//! Conformance harness.

use {
    super::{context::InstrContext, effects::InstrEffects},
    crate::{
        conformance::{
            callback::DefaultCallback,
            setup::{compile_transaction_context, program_runtime_environments, recent_blockhash},
        },
        message_processor::process_message,
    },
    solana_compute_budget::compute_budget::ComputeBudget,
    solana_instruction::error::InstructionError,
    solana_program_runtime::{
        invoke_context::{EnvironmentConfig, InvokeContext},
        loaded_programs::ProgramCacheForTxBatch,
        sysvar_cache::SysvarCache,
    },
    solana_pubkey::Pubkey,
    solana_svm_callback::InvokeContextCallback,
    solana_svm_log_collector::LogCollector,
    solana_svm_timings::ExecuteTimings,
    solana_transaction_error::TransactionError,
    std::rc::Rc,
};
#[cfg(feature = "conformance")]
use {
    crate::conformance::{
        callback::ConformanceCallback,
        programs::{fill_program_cache_from_accounts, new_program_cache_with_builtins},
        setup::sysvar_cache_from_accounts,
    },
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

    let log_collector = LogCollector::new_ref();
    let feature_set = input.feature_set;
    let simd_0268_active = feature_set.raise_cpi_nesting_limit_to_8;

    let mut compute_budget = ComputeBudget::new_with_defaults(simd_0268_active);
    compute_budget.compute_unit_limit = input.cu_avail; // Clamp budget for execution by cu_avail

    let rent = sysvar_cache.get_rent().unwrap();
    let program_id = &input.instruction.program_id;
    let loader_key = program_cache
        .find(program_id)
        .expect("program not loaded in cache")
        .account_owner();

    let (sanitized_message, mut transaction_context) = compile_transaction_context(
        &input.instruction,
        &input.accounts,
        program_id,
        &loader_key,
        &compute_budget,
        (*rent).clone(),
    );

    let runtime_environments = program_runtime_environments(&input.feature_set, &compute_budget);

    let result = {
        let (blockhash, blockhash_lamports_per_signature) = recent_blockhash(sysvar_cache);

        let environment_config = EnvironmentConfig::new(
            blockhash,
            blockhash_lamports_per_signature,
            false,
            callback,
            &feature_set,
            &runtime_environments,
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

    InstrEffects {
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
        let simd_0268_active = feature_set.raise_cpi_nesting_limit_to_8;
        let compute_budget = ComputeBudget::new_with_defaults(simd_0268_active);
        let environments = program_runtime_environments(feature_set, &compute_budget);

        let mut cache = new_program_cache_with_builtins(slot);
        fill_program_cache_from_accounts(
            &mut cache,
            environments.get_env_for_deployment(),
            &instr_context.accounts,
            slot,
        )
        .unwrap();

        cache
    };

    execute_instr_with_callback(
        &instr_context,
        &ConformanceCallback,
        &mut program_cache,
        &sysvar_cache,
    )
    .into()
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
        let context = InstrContext::new_with_default_budget(
            SVMFeatureSet::default(),
            vec![],
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
}
