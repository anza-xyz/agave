//! VM serialization conformance harness.

use {
    crate::conformance::{
        callback::DefaultCallback,
        fd_hash::fd_hash,
        instr::context::InstrContext,
        setup::{
            compile_transaction_context, program_runtime_environments, recent_blockhash,
            sysvar_cache_from_accounts,
        },
    },
    prost::Message,
    protosol::protos::{
        InstrContext as ProtoInstrContext, VmInputMemoryRegion as ProtoVmInputMemoryRegion,
        VmSerializationEffects as ProtoVmSerializationEffects,
        VmSerializedAccountMetadata as ProtoVmSerializedAccountMetadata,
    },
    solana_compute_budget::compute_budget::ComputeBudget,
    solana_program_runtime::{
        invoke_context::{EnvironmentConfig, InvokeContext},
        loaded_programs::ProgramCacheForTxBatch,
        memory_context::SerializedAccountMetadata,
        serialization::serialize_parameters,
        solana_sbpf::memory_region::MemoryRegion,
    },
    solana_svm_log_collector::LogCollector,
    std::ffi::c_int,
};

pub fn execute_vm_serialize(input: ProtoInstrContext) -> ProtoVmSerializationEffects {
    let instr_context = InstrContext::from(input);

    let log_collector = LogCollector::new_ref();
    let feature_set = instr_context.feature_set;
    let virtual_address_space_adjustments = feature_set.virtual_address_space_adjustments;
    let direct_mapping = feature_set.account_data_direct_mapping;
    let direct_account_pointers = feature_set.direct_account_pointers_in_program_input;

    let compute_budget = ComputeBudget::new_with_defaults(feature_set.raise_cpi_nesting_limit_to_8);
    // No CU limit for this harness.

    let sysvar_cache = sysvar_cache_from_accounts(&instr_context.accounts);
    let rent = sysvar_cache.get_rent().unwrap();
    let program_id = instr_context.instruction.program_id;
    let loader_key = instr_context
        .accounts
        .iter()
        .find(|(key, _)| *key == program_id)
        .map(|(_, account)| account.owner)
        .expect("program not found in accounts");

    let (sanitized_message, mut transaction_context) = compile_transaction_context(
        &instr_context.instruction,
        &instr_context.accounts,
        &program_id,
        &loader_key,
        &compute_budget,
        (*rent).clone(),
    );

    // We're only testing the parameter serialization, so use an empty cache.
    let mut program_cache = ProgramCacheForTxBatch::default();

    let runtime_environments = program_runtime_environments(&feature_set, &compute_budget);
    let (blockhash, lamports_per_signature) = recent_blockhash(&sysvar_cache);
    let environment_config = EnvironmentConfig::new(
        blockhash,
        lamports_per_signature,
        false,
        &DefaultCallback,
        &feature_set,
        &runtime_environments,
        &sysvar_cache,
    );

    let mut invoke_context = InvokeContext::new(
        &mut transaction_context,
        &mut program_cache,
        environment_config,
        Some(log_collector.clone()),
        compute_budget.to_budget(),
        compute_budget.to_cost(),
    );

    invoke_context
        .prepare_top_level_instructions(&sanitized_message)
        .unwrap();
    invoke_context.push().unwrap();

    let instruction_context = invoke_context
        .transaction_context
        .get_current_instruction_context()
        .unwrap();

    match serialize_parameters(
        &instruction_context,
        virtual_address_space_adjustments,
        direct_mapping,
        direct_account_pointers,
    ) {
        Ok((aligned_memory, input_memory_regions, account_metadatas, _instruction_data_offset)) => {
            let serialized_memory_hash = fd_hash(0, aligned_memory.as_slice());
            let vm_input_memory_regions = input_memory_regions
                .iter()
                .map(memory_region_to_proto)
                .collect();
            let serialized_account_metadata = account_metadatas
                .iter()
                .map(serialized_acct_meta_to_proto)
                .collect();
            ProtoVmSerializationEffects {
                has_error: false,
                serialized_memory_hash,
                vm_input_memory_regions,
                serialized_account_metadata,
            }
        }
        Err(_) => ProtoVmSerializationEffects {
            has_error: true,
            ..Default::default()
        },
    }
}

fn memory_region_to_proto(region: &MemoryRegion) -> ProtoVmInputMemoryRegion {
    ProtoVmInputMemoryRegion {
        vm_address: region.vm_addr,
        region_size: region.len,
        is_writable: region.writable,
    }
}

fn serialized_acct_meta_to_proto(
    meta: &SerializedAccountMetadata,
) -> ProtoVmSerializedAccountMetadata {
    ProtoVmSerializedAccountMetadata {
        original_data_len: meta.original_data_len as u64,
        vm_data_addr: meta.vm_data_addr,
        vm_key_addr: meta.vm_key_addr,
        vm_lamports_addr: meta.vm_lamports_addr,
        vm_owner_addr: meta.vm_owner_addr,
    }
}

/// # Safety
///
/// `in_ptr` must point to `in_sz` initialized bytes. `out_ptr` must point
/// to a writable buffer of at least `*out_psz` bytes. On return, `*out_psz`
/// is updated to the number of bytes written.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sol_compat_vm_serialize_execute_v1(
    out_ptr: *mut u8,
    out_psz: *mut u64,
    in_ptr: *mut u8,
    in_sz: u64,
) -> c_int {
    let in_slice = unsafe { std::slice::from_raw_parts(in_ptr, in_sz as usize) };
    let Ok(instr_context) = ProtoInstrContext::decode(in_slice) else {
        return 0;
    };

    let effects = execute_vm_serialize(instr_context);
    let out_slice = unsafe { std::slice::from_raw_parts_mut(out_ptr, (*out_psz) as usize) };
    let out_vec = effects.encode_to_vec();
    if out_vec.len() > out_slice.len() {
        return 0;
    }
    out_slice[..out_vec.len()].copy_from_slice(&out_vec);
    unsafe { *out_psz = out_vec.len() as u64 };

    1
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        protosol::protos::{AcctState as ProtoAcctState, InstrAcct as ProtoInstrAcct},
        solana_pubkey::Pubkey,
        solana_rent::Rent,
        solana_sdk_ids::{bpf_loader_deprecated, bpf_loader_upgradeable, sysvar},
    };

    const PROGRAM_ID: [u8; 32] = [7; 32];

    fn account(address: u8, owner: Pubkey, data_len: usize) -> ProtoAcctState {
        ProtoAcctState {
            address: vec![address; 32],
            lamports: 1,
            data: vec![0; data_len],
            executable: false,
            owner: owner.to_bytes().to_vec(),
        }
    }

    fn instr_acct(index: u32, is_writable: bool, is_signer: bool) -> ProtoInstrAcct {
        ProtoInstrAcct {
            index,
            is_writable,
            is_signer,
        }
    }

    fn rent_sysvar() -> ProtoAcctState {
        ProtoAcctState {
            address: sysvar::rent::id().to_bytes().to_vec(),
            lamports: 1,
            data: bincode::serialize(&Rent::default()).unwrap(),
            executable: false,
            owner: sysvar::id().to_bytes().to_vec(),
        }
    }

    fn serialize(
        mut accounts: Vec<ProtoAcctState>,
        instr_accounts: Vec<ProtoInstrAcct>,
    ) -> ProtoVmSerializationEffects {
        accounts.push(rent_sysvar()); // <-- Loads Rent into SysvarCache
        execute_vm_serialize(ProtoInstrContext {
            program_id: PROGRAM_ID.to_vec(),
            accounts,
            instr_accounts,
            ..Default::default()
        })
    }

    #[test]
    fn test_serialize_single_account() {
        let program_id = Pubkey::new_from_array(PROGRAM_ID);
        let effects = serialize(
            vec![
                account(1, program_id, 8),
                account(PROGRAM_ID[0], bpf_loader_upgradeable::id(), 0),
            ],
            vec![instr_acct(0, true, false)],
        );
        assert!(!effects.has_error);
        assert_eq!(effects.serialized_account_metadata.len(), 1);
    }

    #[test]
    fn test_serialize_multiple_accounts() {
        let program_id = Pubkey::new_from_array(PROGRAM_ID);
        let effects = serialize(
            vec![
                account(1, program_id, 4),
                account(2, program_id, 16),
                account(PROGRAM_ID[0], bpf_loader_upgradeable::id(), 0),
            ],
            vec![instr_acct(0, true, true), instr_acct(1, false, false)],
        );
        assert!(!effects.has_error);
        assert_eq!(effects.serialized_account_metadata.len(), 2);
    }

    #[test]
    fn test_serialize_deprecated_loader_program() {
        // A program owned by the deprecated loader exercises the unaligned
        // serialization path.
        let program_id = Pubkey::new_from_array(PROGRAM_ID);
        let effects = serialize(
            vec![
                account(1, program_id, 8),
                account(PROGRAM_ID[0], bpf_loader_deprecated::id(), 0),
            ],
            vec![instr_acct(0, true, false)],
        );
        assert!(!effects.has_error);
        assert_eq!(effects.serialized_account_metadata.len(), 1);
    }
}
