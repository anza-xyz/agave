//! Instruction conformance harness.

use {
    agave_precompiles::is_precompile,
    prost::Message,
    protosol::protos::{InstrContext as ProtoInstrContext, InstrEffects as ProtoInstrEffects},
    solana_svm::conformance::{
        callback::ConformanceCallback,
        direct_mapping::direct_mapping_handle_cu_exhaustion,
        instr::{context::InstrContext, harness::execute_instr_with_callback},
        programs::{fill_program_cache_from_accounts, new_program_cache_with_builtins},
        setup::{compute_budget, program_runtime_environments, sysvar_cache_from_accounts},
    },
    std::ffi::c_int,
};

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
        &ConformanceCallback::default(),
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
    let cu_avail = effects.cu_avail;
    let has_err = effects.result.is_some();
    let mut result: ProtoInstrEffects = effects.into();

    direct_mapping_handle_cu_exhaustion(
        instr_context.feature_set.virtual_address_space_adjustments,
        cu_avail,
        has_err,
        result.modified_accounts.iter_mut(),
    );

    result
}

/// # Safety
///
/// `in_ptr` must point to `in_sz` initialized bytes. `out_ptr` must point
/// to a writable buffer of at least `*out_psz` bytes. On return, `*out_psz`
/// is updated to the number of bytes written.
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
        super::*, protosol::protos::acct_state::DataRepr, solana_account::Account,
        solana_pubkey::Pubkey, solana_svm::conformance::programs::keyed_account_for_system_program,
        solana_system_program::system_processor::DEFAULT_COMPUTE_UNITS as SYSTEM_TRANSFER_CUS,
    };

    const FROM_BASE_LAMPORTS: u64 = 5_000;
    const TO_BASE_LAMPORTS: u64 = 1_000;

    fn system_account_with_lamports(lamports: u64) -> Account {
        Account {
            lamports,
            data: vec![],
            owner: solana_sdk_ids::system_program::id(),
            executable: false,
            rent_epoch: u64::MAX,
        }
    }

    fn proto_account(pubkey: Pubkey, account: Account) -> protosol::protos::AcctState {
        protosol::protos::AcctState {
            address: pubkey.to_bytes().to_vec(),
            owner: account.owner.to_bytes().to_vec(),
            lamports: account.lamports,
            data_repr: Some(DataRepr::Data(account.data)),
            executable: account.executable,
        }
    }

    fn proto_sysvar_account<T: serde::Serialize>(
        pubkey: Pubkey,
        sysvar: &T,
    ) -> protosol::protos::AcctState {
        protosol::protos::AcctState {
            address: pubkey.to_bytes().to_vec(),
            owner: solana_sdk_ids::sysvar::id().to_bytes().to_vec(),
            lamports: 1,
            data_repr: Some(DataRepr::Data(bincode::serialize(sysvar).unwrap())),
            executable: false,
        }
    }

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
