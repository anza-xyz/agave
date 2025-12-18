#![allow(clippy::missing_safety_doc)]

use {
    crate::{
        execute_instr,
        fixture::{
            instr_context::InstrContext,
            proto::{InstrContext as ProtoInstrContext, InstrEffects as ProtoInstrEffects},
        },
    },
    agave_feature_set::{increase_cpi_account_info_limit, raise_cpi_nesting_limit_to_8},
    agave_syscalls::create_program_runtime_environment_v1,
    prost::Message,
    solana_compute_budget::compute_budget::ComputeBudget,
    solana_program_runtime::loaded_programs::ProgramRuntimeEnvironments,
    std::{env, ffi::c_int, sync::Arc},
};

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sol_compat_init(_log_level: i32) {
    unsafe {
        env::set_var("SOLANA_RAYON_THREADS", "1");
        env::set_var("RAYON_NUM_THREADS", "1");
    }
    if env::var("ENABLE_SOLANA_LOGGER").is_ok() {
        /* Pairs with RUST_LOG={trace,debug,info,etc} */
        agave_logger::setup();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sol_compat_fini() {}

pub fn execute_instr_proto(input: ProtoInstrContext) -> Option<ProtoInstrEffects> {
    let cu_avail = input.cu_avail;

    let Ok(instr_context) = InstrContext::try_from(input) else {
        return None;
    };

    let feature_set = &instr_context.feature_set;
    let simd_0268_active = feature_set.is_active(&raise_cpi_nesting_limit_to_8::id());
    let simd_0339_active = feature_set.is_active(&increase_cpi_account_info_limit::id());

    let compute_budget = {
        let mut budget = ComputeBudget::new_with_defaults(simd_0268_active, simd_0339_active);
        budget.compute_unit_limit = cu_avail;
        budget
    };

    // When testing with protobuf, we fill the sysvar cache from input accounts.
    let sysvar_cache = {
        let mut cache = solana_program_runtime::sysvar_cache::SysvarCache::default();
        crate::sysvar_cache::fill_from_accounts(&mut cache, &instr_context.accounts);
        cache
    };

    // When testing with protobuf, we fill the program cache from input accounts.
    let mut program_cache = {
        let slot = sysvar_cache.get_clock().unwrap().slot;
        let environments = ProgramRuntimeEnvironments {
            program_runtime_v1: Arc::new(
                create_program_runtime_environment_v1(
                    &instr_context.feature_set.runtime_features(),
                    &compute_budget.to_budget(),
                    false, /* deployment */
                    false, /* debugging_features */
                )
                .unwrap(),
            ),
            ..ProgramRuntimeEnvironments::default()
        };

        let mut cache = crate::program_cache::new_with_builtins(&instr_context.feature_set, slot);
        crate::program_cache::fill_from_accounts(
            &mut cache,
            &environments,
            &instr_context.accounts,
            slot,
        )
        .unwrap();

        cache
    };

    let instr_effects = execute_instr(
        instr_context,
        &compute_budget,
        &mut program_cache,
        &sysvar_cache,
    );
    instr_effects.map(Into::into)
}

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
    let Some(instr_effects) = execute_instr_proto(instr_context) else {
        return 0;
    };
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
