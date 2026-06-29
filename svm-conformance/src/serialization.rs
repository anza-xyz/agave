//! VM serialization conformance harness.

use {
    crate::{
        callback::DefaultCallback,
        fd_hash::fd_hash,
        instr::context::InstrContext,
        setup::{
            InvokeContextFields, compute_budget, prepare_invoke_context_fields, program_loader_key,
            program_runtime_environments, sysvar_cache_from_accounts,
        },
    },
    prost::Message,
    protosol::protos::{
        InstrContext as ProtoInstrContext, VmInputMemoryRegion as ProtoVmInputMemoryRegion,
        VmSerializationEffects as ProtoVmSerializationEffects,
        VmSerializedAccountMetadata as ProtoVmSerializedAccountMetadata,
    },
    solana_instruction::error::InstructionError,
    solana_message::SanitizedMessage,
    solana_program_runtime::{
        invoke_context::InvokeContext,
        loaded_programs::ProgramCacheForTxBatch,
        memory_context::SerializedAccountMetadata,
        serialization::serialize_parameters,
        solana_sbpf::{
            aligned_memory::AlignedMemory, ebpf::HOST_ALIGN, memory_region::MemoryRegion,
        },
    },
    solana_svm_feature_set::SVMFeatureSet,
    std::ffi::c_int,
};

pub fn execute_vm_serialize(input: ProtoInstrContext) -> ProtoVmSerializationEffects {
    let instr_context = InstrContext::from(input);

    let feature_set = instr_context.feature_set;

    let compute_budget = compute_budget(&feature_set);
    // No CU limit for this harness.

    let sysvar_cache = sysvar_cache_from_accounts(&instr_context.accounts);
    let program_id = instr_context.instruction.program_id;
    let loader_key = program_loader_key(&instr_context.accounts, &program_id);

    let program_runtime_environments = program_runtime_environments(&feature_set, &compute_budget);

    // We're only testing the parameter serialization, so use an empty cache.
    let mut program_cache = ProgramCacheForTxBatch::default();

    let InvokeContextFields {
        sanitized_message,
        mut transaction_context,
        environment_config,
        log_collector,
        execution_budget,
        execution_cost,
    } = prepare_invoke_context_fields(
        &instr_context,
        &DefaultCallback,
        &loader_key,
        &sysvar_cache,
        &compute_budget,
        &program_runtime_environments,
    );

    let mut invoke_context = InvokeContext::new(
        &mut transaction_context,
        &mut program_cache,
        environment_config,
        Some(log_collector.clone()),
        execution_budget,
        execution_cost,
    );

    match push_and_serialize_parameters(&mut invoke_context, &sanitized_message, &feature_set) {
        Ok(SerializedParameters {
            aligned_memory,
            input_memory_regions,
            account_metadata,
        }) => {
            let serialized_memory_hash = fd_hash(0, aligned_memory.as_slice());
            let vm_input_memory_regions = input_memory_regions
                .iter()
                .map(memory_region_to_proto)
                .collect();
            let serialized_account_metadata = account_metadata
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

/// The product of serializing a program's input parameters into VM memory: the
/// serialized region itself plus the metadata needed to map accounts back out.
pub(crate) struct SerializedParameters {
    pub(crate) aligned_memory: AlignedMemory<HOST_ALIGN>,
    pub(crate) input_memory_regions: Vec<MemoryRegion>,
    pub(crate) account_metadata: Vec<SerializedAccountMetadata>,
}

/// Push the message's single top-level instruction onto `invoke_context`, then
/// serialize that instruction's program input parameters into VM memory.
pub(crate) fn push_and_serialize_parameters<'ix_data>(
    invoke_context: &mut InvokeContext<'_, 'ix_data>,
    sanitized_message: &'ix_data SanitizedMessage,
    feature_set: &SVMFeatureSet,
) -> Result<SerializedParameters, InstructionError> {
    invoke_context
        .prepare_top_level_instructions(sanitized_message)
        .expect("failed to prepare top-level instructions");
    invoke_context
        .push()
        .expect("failed to push instruction context");

    let instruction_context = invoke_context
        .transaction_context
        .get_current_instruction_context()
        .unwrap();
    serialize_parameters(
        &instruction_context,
        feature_set.virtual_address_space_adjustments,
        feature_set.account_data_direct_mapping,
        feature_set.direct_account_pointers_in_program_input,
    )
    .map(
        |(aligned_memory, input_memory_regions, account_metadata, _instruction_data_offset)| {
            SerializedParameters {
                aligned_memory,
                input_memory_regions,
                account_metadata,
            }
        },
    )
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
