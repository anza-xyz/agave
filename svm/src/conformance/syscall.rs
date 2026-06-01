//! VM syscall conformance harness.

use {
    crate::conformance::{
        callback::DefaultCallback,
        fd_err_map::unpack_stable_result,
        instr::context::InstrContext,
        programs::{fill_program_cache_from_accounts, new_program_cache_with_builtins},
        setup::{
            compile_transaction_context, program_loader_key, program_runtime_environments,
            recent_blockhash, sysvar_cache_from_accounts,
        },
    },
    prost::Message,
    protosol::protos::{
        InputDataRegion as ProtoInputDataRegion, SyscallContext as ProtoSyscallContext,
        SyscallEffects as ProtoSyscallEffects, SyscallInvocation as ProtoSyscallInvocation,
        VmContext as ProtoVmContext,
    },
    solana_compute_budget::compute_budget::ComputeBudget,
    solana_program_runtime::{
        invoke_context::{BpfAllocator, EnvironmentConfig, InvokeContext},
        loaded_programs::ProgramCacheForTxBatch,
        memory_context::MemoryContext,
        serialization::serialize_parameters,
        solana_sbpf::{
            aligned_memory::AlignedMemory,
            ebpf::{HOST_ALIGN, MM_BYTECODE_START, MM_HEAP_START, MM_INPUT_START, MM_STACK_START},
            error::{ProgramResult, StableResult},
            memory_region::{MemoryMapping, MemoryRegion},
            program::{BuiltinProgram, SBPFVersion},
            vm::{ContextObject, EbpfVm},
        },
    },
    solana_pubkey::Pubkey,
    std::{ffi::c_int, sync::Arc},
};

const STACK_GAP_SIZE: u64 = 4_096;
const STACK_SIZE: usize = 64 * STACK_GAP_SIZE as usize;
/// Upper bound on `vm_context.heap_max` — matches Firedancer's cap so the same
/// fuzzer inputs run on either implementation.
const HEAP_MAX: usize = 256 * 1024;
const SBPF_VERSION: SBPFVersion = SBPFVersion::V0;

pub fn execute_vm_syscall(input: ProtoSyscallContext) -> ProtoSyscallEffects {
    let instr_context = InstrContext::from(input.instr_ctx.expect("missing instr context"));
    let vm_context = input.vm_ctx.expect("missing vm context");
    let syscall_invocation = input.syscall_invocation.unwrap_or_default();
    let registers = get_registers(&vm_context);

    let feature_set = instr_context.feature_set;
    let virtual_address_space_adjustments = feature_set.virtual_address_space_adjustments;
    let account_data_direct_mapping = feature_set.account_data_direct_mapping;
    let direct_account_pointers_in_program_input =
        feature_set.direct_account_pointers_in_program_input;

    let mut compute_budget =
        ComputeBudget::new_with_defaults(feature_set.raise_cpi_nesting_limit_to_8);
    compute_budget.compute_unit_limit = instr_context.cu_avail; // Clamp budget for execution by cu_avail

    let sysvar_cache = sysvar_cache_from_accounts(&instr_context.accounts);

    let rent = sysvar_cache.get_rent().unwrap();
    let program_id = instr_context.instruction.program_id;
    let loader_key = program_loader_key(&instr_context.accounts, &program_id);

    let (sanitized_message, mut transaction_context) = compile_transaction_context(
        &instr_context.instruction,
        &instr_context.accounts,
        &program_id,
        &loader_key,
        &compute_budget,
        (*rent).clone(),
    );

    // Replay any prior return data the fuzzer wants in scope before the syscall.
    if let Some(return_data) = vm_context.return_data {
        let return_program_id =
            Pubkey::try_from(return_data.program_id).expect("invalid return data program id");
        transaction_context
            .set_return_data(return_program_id, return_data.data)
            .expect("failed to set return data");
    }

    let environments = program_runtime_environments(&feature_set, &compute_budget);

    // Only build out the program cache if the syscall is CPI.
    let mut program_cache = if contains_cpi(&syscall_invocation) {
        let mut cache = new_program_cache_with_builtins(0);
        fill_program_cache_from_accounts(
            &mut cache,
            environments.get_env_for_deployment(),
            &instr_context.accounts,
            0,
        )
        .expect("failed to fill program cache from accounts");
        cache
    } else {
        ProgramCacheForTxBatch::default()
    };

    let execution_environment = environments.get_env_for_execution();
    let config = execution_environment.get_config().clone();
    let syscall_function = execution_environment
        .get_function_registry()
        .lookup_by_name(&syscall_invocation.function_name)
        .expect("syscall function not registered")
        .1
        .0;

    let (blockhash, lamports_per_signature) = recent_blockhash(&sysvar_cache);
    let environment_config = EnvironmentConfig::new(
        blockhash,
        lamports_per_signature,
        false,
        &DefaultCallback,
        &feature_set,
        &environments,
        &sysvar_cache,
    );

    let mut invoke_context = InvokeContext::new(
        &mut transaction_context,
        &mut program_cache,
        environment_config,
        None,
        compute_budget.to_budget(),
        compute_budget.to_cost(),
    );

    invoke_context
        .prepare_top_level_instructions(&sanitized_message)
        .expect("failed to prepare top-level instructions");
    invoke_context
        .push()
        .expect("failed to push instruction context");

    let caller_instr_context = invoke_context
        .transaction_context
        .get_current_instruction_context()
        .unwrap();
    let (_aligned_memory, input_memory_regions, accounts_metadata, _instruction_data_offset) =
        serialize_parameters(
            &caller_instr_context,
            virtual_address_space_adjustments,
            account_data_direct_mapping,
            direct_account_pointers_in_program_input,
        )
        .expect("failed to serialize parameters");

    assert!(
        vm_context.heap_max as usize <= HEAP_MAX,
        "vm_context.heap_max ({}) exceeds HEAP_MAX ({HEAP_MAX})",
        vm_context.heap_max,
    );
    let rodata = AlignedMemory::<HOST_ALIGN>::from(&vm_context.rodata);
    let mut stack = AlignedMemory::<HOST_ALIGN>::from(&vec![0; STACK_SIZE]);
    let mut heap = AlignedMemory::<HOST_ALIGN>::from(&vec![0; vm_context.heap_max as usize]);

    copy_memory_prefix(heap.as_slice_mut(), &syscall_invocation.heap_prefix);
    copy_memory_prefix(stack.as_slice_mut(), &syscall_invocation.stack_prefix);

    let stack_frame_gap = if SBPF_VERSION.stack_frame_gaps() && config.enable_stack_frame_gaps {
        config.stack_frame_size as u64
    } else {
        0
    };
    let regions = [
        MemoryRegion::new(rodata.as_slice() as *const [u8], MM_BYTECODE_START),
        MemoryRegion::new_gapped(
            stack.as_slice_mut() as *mut [u8],
            MM_STACK_START,
            stack_frame_gap,
        ),
        MemoryRegion::new(heap.as_slice_mut() as *mut [u8], MM_HEAP_START),
    ]
    .into_iter()
    .chain(input_memory_regions)
    .collect();
    // SAFETY: the backing memory for rodata/stack/heap (and any input regions)
    // lives until the end of this function, which outlives the `MemoryMapping`.
    let memory_mapping = unsafe {
        MemoryMapping::new_with_access_violation_handler(
            regions,
            &config,
            SBPF_VERSION,
            invoke_context.transaction_context.access_violation_handler(
                virtual_address_space_adjustments,
                account_data_direct_mapping,
            ),
        )
    }
    .expect("failed to create memory mapping");

    invoke_context
        .memory_contexts
        .set_memory_context_abi_v1(MemoryContext::new(
            BpfAllocator::new(vm_context.heap_max),
            accounts_metadata,
            memory_mapping,
        ))
        .expect("failed to set memory context");

    let (program_result, call_depth) = {
        // Invoke the syscall with a `&'static` InvokeContext, then take the
        // result, dropping the VM. Avoids dangling memory.
        let loader = Arc::new(BuiltinProgram::new_loader(config));
        let invoke_context_static: &mut InvokeContext<'static, 'static> =
            unsafe { std::mem::transmute(&mut invoke_context) };

        let mut vm = EbpfVm::new(loader, SBPF_VERSION, invoke_context_static, STACK_SIZE);
        vm.registers = registers;

        vm.invoke_function(syscall_function);

        let program_result = std::mem::replace(&mut vm.program_result, ProgramResult::Ok(0));
        let call_depth = vm.call_depth;
        (program_result, call_depth)
    };

    let input_data_regions = extract_input_data_regions(
        &invoke_context,
        &program_result,
        virtual_address_space_adjustments,
    );

    let unpacked = unpack_stable_result(program_result);
    let cu_avail = invoke_context.get_remaining();
    invoke_context
        .pop()
        .expect("failed to pop instruction context");

    ProtoSyscallEffects {
        error: unpacked.error,
        error_kind: unpacked.error_kind,
        r0: unpacked.r0,
        cu_avail,
        heap: heap.as_slice().to_vec(),
        stack: stack.as_slice().to_vec(),
        input_data_regions,
        frame_count: call_depth,
        rodata: rodata.as_slice().to_vec(),
        pc: 0,
        ..Default::default()
    }
}

fn contains_cpi(syscall_invocation: &ProtoSyscallInvocation) -> bool {
    syscall_invocation.function_name == b"sol_invoke_signed_c"
        || syscall_invocation.function_name == b"sol_invoke_signed_rust"
}

fn copy_memory_prefix(dst: &mut [u8], src: &[u8]) {
    let size = dst.len().min(src.len());
    dst[..size].copy_from_slice(&src[..size]);
}

fn get_registers(vm_context: &ProtoVmContext) -> [u64; 12] {
    [
        vm_context.r0,
        vm_context.r1,
        vm_context.r2,
        vm_context.r3,
        vm_context.r4,
        vm_context.r5,
        vm_context.r6,
        vm_context.r7,
        vm_context.r8,
        vm_context.r9,
        vm_context.r10,
        vm_context.r11,
    ]
}

fn extract_input_data_regions(
    invoke_context: &InvokeContext,
    program_result: &ProgramResult,
    virtual_address_space_adjustments: bool,
) -> Vec<ProtoInputDataRegion> {
    // When virtual_address_space_adjustments is enabled, Agave calls
    // update_caller_account_region only after a _successful_ CPI execution, so
    // on failure the input regions can hold stale data — return empty instead.
    if virtual_address_space_adjustments && matches!(program_result, StableResult::Err(_)) {
        return Vec::new();
    }
    invoke_context
        .memory_contexts
        .memory_mapping()
        .ok()
        .map(|mapping| {
            let mut regions: Vec<ProtoInputDataRegion> = mapping
                .get_regions()
                .iter()
                .filter(|region| region.vm_addr >= MM_INPUT_START)
                .map(mem_region_to_input_data_region)
                .collect();
            regions.sort_by_key(|region| region.offset);
            regions
        })
        .unwrap_or_default()
}

fn mem_region_to_input_data_region(region: &MemoryRegion) -> ProtoInputDataRegion {
    ProtoInputDataRegion {
        content: unsafe {
            std::slice::from_raw_parts(region.host_addr as *const u8, region.len as usize).to_vec()
        },
        offset: region.vm_addr.saturating_sub(MM_INPUT_START),
        is_writable: region.writable,
    }
}

/// # Safety
///
/// `in_ptr` must point to `in_sz` initialized bytes. `out_ptr` must point
/// to a writable buffer of at least `*out_psz` bytes. On return, `*out_psz`
/// is updated to the number of bytes written.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sol_compat_vm_syscall_execute_v1(
    out_ptr: *mut u8,
    out_psz: *mut u64,
    in_ptr: *mut u8,
    in_sz: u64,
) -> c_int {
    let in_slice = unsafe { std::slice::from_raw_parts(in_ptr, in_sz as usize) };
    let Ok(syscall_context) = ProtoSyscallContext::decode(in_slice) else {
        return 0;
    };

    let syscall_effects = execute_vm_syscall(syscall_context);
    let out_slice = unsafe { std::slice::from_raw_parts_mut(out_ptr, (*out_psz) as usize) };
    let out_vec = syscall_effects.encode_to_vec();
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
        protosol::protos::{
            AcctState as ProtoAcctState, InstrContext as ProtoInstrContext,
            SyscallInvocation as ProtoSyscallInvocation, VmContext as ProtoVmContext,
        },
        solana_rent::Rent,
        solana_sdk_ids::sysvar,
    };

    const PROGRAM_ID: [u8; 32] = [7; 32];

    fn syscall_context(
        function_name: &[u8],
        r1: u64,
        r2: u64,
        r3: u64,
        r4: u64,
        heap_prefix: Vec<u8>,
    ) -> ProtoSyscallContext {
        let program_account = ProtoAcctState {
            address: PROGRAM_ID.to_vec(),
            lamports: 0,
            data: vec![],
            executable: true,
            owner: Pubkey::default().to_bytes().to_vec(),
        };
        let rent_sysvar = ProtoAcctState {
            address: sysvar::rent::id().to_bytes().to_vec(),
            lamports: 1,
            data: bincode::serialize(&Rent::default()).unwrap(),
            executable: false,
            owner: sysvar::id().to_bytes().to_vec(),
        };
        ProtoSyscallContext {
            instr_ctx: Some(ProtoInstrContext {
                program_id: PROGRAM_ID.to_vec(),
                accounts: vec![program_account, rent_sysvar],
                instr_accounts: vec![],
                data: vec![],
                cu_avail: 200_000,
                features: None,
            }),
            vm_ctx: Some(ProtoVmContext {
                heap_max: 1024,
                r1,
                r2,
                r3,
                r4,
                ..Default::default()
            }),
            syscall_invocation: Some(ProtoSyscallInvocation {
                function_name: function_name.to_vec(),
                heap_prefix,
                stack_prefix: vec![],
            }),
        }
    }

    #[test]
    fn test_sol_log() {
        let msg = b"hello";
        let effects = execute_vm_syscall(syscall_context(
            b"sol_log_",
            MM_HEAP_START,    // r1: msg address in heap
            msg.len() as u64, // r2: length
            0,
            0,
            msg.to_vec(),
        ));

        assert_eq!(effects.error, 0);
        // Logs are no longer collected (the harness runs without a log
        // collector), so the syscall succeeding is all we assert here.
        assert!(effects.cu_avail < 200_000, "syscall should consume compute");
    }

    #[test]
    fn test_sol_memset() {
        let effects = execute_vm_syscall(syscall_context(
            b"sol_memset_",
            MM_HEAP_START, // r1: dst
            0x42,          // r2: byte
            8,             // r3: count
            0,
            vec![0u8; 16],
        ));

        assert_eq!(effects.error, 0);
        assert_eq!(&effects.heap[..8], &[0x42; 8]);
        assert_eq!(
            &effects.heap[8..16],
            &[0u8; 8],
            "must not write past length"
        );
    }

    #[test]
    fn test_sol_panic_surfaces_error() {
        let effects = execute_vm_syscall(syscall_context(
            b"sol_panic_",
            MM_HEAP_START, // r1: file address
            1,             // r2: file length
            10,            // r3: line
            5,             // r4: column
            b"x".to_vec(),
        ));

        assert_ne!(effects.error, 0);
    }
}
