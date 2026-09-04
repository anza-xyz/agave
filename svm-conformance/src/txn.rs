//! Transaction conformance harness.

use {
    agave_feature_set::virtual_address_space_adjustments,
    agave_precompiles::is_precompile,
    prost::Message,
    protosol::protos::{TxnContext as ProtoTxnContext, TxnResult as ProtoTxnResult},
    solana_instruction::error::InstructionError,
    solana_svm::conformance::{
        callback::ConformanceCallback,
        direct_mapping::direct_mapping_handle_cu_exhaustion,
        programs::{fill_program_cache_from_accounts, new_program_cache_with_builtins},
        setup::{
            compute_budget as default_compute_budget, program_runtime_environments,
            sysvar_cache_from_accounts,
        },
        txn::{context::TxnContext, harness::execute_txn_with_callback},
    },
    solana_transaction_error::TransactionError,
    std::ffi::c_int,
};

pub fn execute_txn_proto(input: ProtoTxnContext) -> ProtoTxnResult {
    let epoch_total_stake = input
        .bank
        .as_ref()
        .map(|bank| bank.total_epoch_stake)
        .unwrap_or_default();
    let context = TxnContext::from(input);

    let sysvar_cache = sysvar_cache_from_accounts(&context.accounts);
    let mut program_cache = {
        let slot = sysvar_cache.get_clock().unwrap().slot;
        let runtime_features = context.feature_set.runtime_features();
        let compute_budget = default_compute_budget(&runtime_features);
        let environments = program_runtime_environments(&runtime_features, &compute_budget);

        let accounts = context.accounts.clone();

        let mut cache = new_program_cache_with_builtins(slot);
        fill_program_cache_from_accounts(
            &mut cache,
            environments.get_env_for_execution(),
            &accounts,
            slot,
        );

        cache
    };

    let callback = ConformanceCallback { epoch_total_stake };
    let mut effects =
        execute_txn_with_callback(&context, &callback, &mut program_cache, &sysvar_cache);

    let precompile_custom_error_index = match &effects.status {
        Err(TransactionError::InstructionError(index, InstructionError::Custom(_))) => context
            .message
            .instructions()
            .get(usize::from(*index))
            .and_then(|instruction| {
                context
                    .message
                    .static_account_keys()
                    .get(usize::from(instruction.program_id_index))
            })
            .is_some_and(|program_id| is_precompile(program_id, |_| true))
            .then_some(*index),
        _ => None,
    };

    if let Some(index) = precompile_custom_error_index {
        effects.status = Err(TransactionError::InstructionError(
            index,
            InstructionError::Custom(0),
        ));
    }

    let cu_avail = effects.cu_avail;
    let has_err = effects.status.is_err();
    let mut result: ProtoTxnResult = effects.into();

    direct_mapping_handle_cu_exhaustion(
        context
            .feature_set
            .is_active(&virtual_address_space_adjustments::id()),
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
pub unsafe extern "C" fn sol_compat_svm_txn_execute_v1(
    out_ptr: *mut u8,
    out_psz: *mut u64,
    in_ptr: *mut u8,
    in_sz: u64,
) -> c_int {
    if in_ptr.is_null() || out_ptr.is_null() || out_psz.is_null() {
        return 0;
    }
    let in_slice = unsafe { std::slice::from_raw_parts(in_ptr, in_sz as usize) };
    let Ok(txn_context) = ProtoTxnContext::decode(in_slice) else {
        return 0;
    };
    let txn_result = execute_txn_proto(txn_context);
    let out_slice = unsafe { std::slice::from_raw_parts_mut(out_ptr, (*out_psz) as usize) };
    let out_vec = txn_result.encode_to_vec();
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
        protosol::protos::{
            AcctState as ProtoAcctState, CompiledInstruction as ProtoCompiledInstruction,
            MessageHeader as ProtoMessageHeader, SanitizedTransaction as ProtoSanitizedTransaction,
            TransactionMessage as ProtoTransactionMessage, acct_state::DataRepr,
        },
        solana_clock::Clock,
        solana_pubkey::Pubkey,
        solana_sdk_ids::{system_program, sysvar},
        solana_svm::conformance::programs::keyed_account_for_system_program,
        solana_system_interface::instruction::transfer,
    };

    #[test]
    fn test_execute_txn_proto() {
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();
        let c = Pubkey::new_unique();

        let clock_pubkey = sysvar::clock::id();
        let blockhash = [0u8; 32];
        let instruction = transfer(&a, &b, 1);
        let (system_program_id, system_program_account) = keyed_account_for_system_program();

        let result = execute_txn_proto(ProtoTxnContext {
            tx: Some(ProtoSanitizedTransaction {
                message: Some(ProtoTransactionMessage {
                    is_legacy: true,
                    header: Some(ProtoMessageHeader {
                        num_required_signatures: 1,
                        num_readonly_signed_accounts: 0,
                        num_readonly_unsigned_accounts: 1,
                    }),
                    account_keys: vec![
                        a.to_bytes().to_vec(),
                        b.to_bytes().to_vec(),
                        system_program::id().to_bytes().to_vec(),
                    ],
                    recent_blockhash: blockhash.to_vec(),
                    instructions: vec![ProtoCompiledInstruction {
                        program_id_index: 2,
                        accounts: vec![0, 1],
                        data: instruction.data,
                    }],
                    address_table_lookups: vec![],
                }),
                message_hash: vec![],
                signatures: vec![],
            }),
            account_shared_data: vec![
                ProtoAcctState {
                    address: c.to_bytes().to_vec(),
                    lamports: 1,
                    data_repr: Some(DataRepr::Data(vec![])),
                    executable: false,
                    owner: system_program::id().to_bytes().to_vec(),
                },
                ProtoAcctState {
                    address: b.to_bytes().to_vec(),
                    lamports: 1,
                    data_repr: Some(DataRepr::Data(vec![])),
                    executable: false,
                    owner: system_program::id().to_bytes().to_vec(),
                },
                ProtoAcctState {
                    address: a.to_bytes().to_vec(),
                    lamports: 1,
                    data_repr: Some(DataRepr::Data(vec![])),
                    executable: false,
                    owner: system_program::id().to_bytes().to_vec(),
                },
                ProtoAcctState {
                    address: clock_pubkey.to_bytes().to_vec(),
                    lamports: 1,
                    data_repr: Some(DataRepr::Data(
                        bincode::serialize(&Clock {
                            slot: 1,
                            ..Clock::default()
                        })
                        .unwrap(),
                    )),
                    executable: false,
                    owner: sysvar::id().to_bytes().to_vec(),
                },
                ProtoAcctState {
                    address: system_program_id.to_bytes().to_vec(),
                    lamports: system_program_account.lamports,
                    data_repr: Some(DataRepr::Data(system_program_account.data)),
                    executable: system_program_account.executable,
                    owner: system_program_account.owner.to_bytes().to_vec(),
                },
            ],
            bank: None,
        });

        assert!(result.executed);
        assert_eq!(
            result
                .modified_accounts
                .iter()
                .map(|account| Pubkey::try_from(account.address.as_slice()).unwrap())
                .collect::<Vec<_>>(),
            vec![c, b, a, clock_pubkey, system_program_id]
        );
    }
}
