use {
    crate::{
        load_builtins,
        proto::{SyscallContext, SyscallEffects},
        InstrContext,
    },
    prost::Message,
    solana_bpf_loader_program::syscalls::create_program_runtime_environment_v1,
    solana_program_runtime::{
        compute_budget::ComputeBudget,
        invoke_context::InvokeContext,
        loaded_programs::ProgramCacheForTxBatch,
        solana_rbpf::{
            ebpf::{self, MM_INPUT_START},
            error::EbpfError,
            memory_region::{MemoryMapping, MemoryRegion},
            program::{BuiltinProgram, SBPFVersion},
            vm::{Config, ContextObject, EbpfVm},
        },
        sysvar_cache::SysvarCache,
    },
    solana_sdk::{
        account::AccountSharedData,
        rent::Rent,
        transaction_context::{TransactionAccount, TransactionContext},
    },
    std::{ffi::c_int, sync::Arc},
};

const STACK_SIZE: usize = 524288; // FD_VM_STACK_MAX

#[no_mangle]
pub unsafe extern "C" fn sol_compat_vm_syscall_execute_v1(
    out_ptr: *mut u8,
    out_psz: *mut u64,
    in_ptr: *mut u8,
    in_sz: u64,
) -> c_int {
    let in_slice = std::slice::from_raw_parts(in_ptr, in_sz as usize);
    let Ok(syscall_context) = SyscallContext::decode(in_slice) else {
        return 0;
    };

    let Some(syscall_effects) = execute_vm_syscall(syscall_context) else {
        return 0;
    };
    let out_slice = std::slice::from_raw_parts_mut(out_ptr, (*out_psz) as usize);
    let out_vec = syscall_effects.encode_to_vec();
    if out_vec.len() > out_slice.len() {
        return 0;
    }
    out_slice[..out_vec.len()].copy_from_slice(&out_vec);
    *out_psz = out_vec.len() as u64;

    1
}

fn execute_vm_syscall(input: SyscallContext) -> Option<SyscallEffects> {
    let instr_ctx: InstrContext = input.instr_ctx?.try_into().ok()?;

    // Create invoke context
    // TODO: factor this into common code with lib.rs
    let mut transaction_accounts =
        Vec::<TransactionAccount>::with_capacity(instr_ctx.accounts.len().saturating_add(1));
    #[allow(deprecated)]
    instr_ctx
        .accounts
        .clone()
        .into_iter()
        .map(|(pubkey, account)| (pubkey, AccountSharedData::from(account)))
        .for_each(|x| transaction_accounts.push(x));

    let compute_budget = ComputeBudget {
        compute_unit_limit: instr_ctx.cu_avail,
        ..ComputeBudget::default()
    };
    let mut transaction_context = TransactionContext::new(
        transaction_accounts.clone(),
        Rent::default(),
        compute_budget.max_invoke_stack_height,
        compute_budget.max_instruction_trace_length,
    );

    // sigh ... What is this mess?
    let mut programs_loaded_for_tx_batch = ProgramCacheForTxBatch::default();
    load_builtins(&mut programs_loaded_for_tx_batch);
    let mut programs_modified_by_tx = ProgramCacheForTxBatch::default();

    let sysvar_cache = SysvarCache::default();
    #[allow(deprecated)]
    let (blockhash, lamports_per_signature) = sysvar_cache
        .get_recent_blockhashes()
        .ok()
        .and_then(|x| (*x).last().cloned())
        .map(|x| (x.blockhash, x.fee_calculator.lamports_per_signature))
        .unwrap_or_default();

    let mut invoke_context = InvokeContext::new(
        &mut transaction_context,
        &sysvar_cache,
        None,
        compute_budget,
        &programs_loaded_for_tx_batch,
        &mut programs_modified_by_tx,
        Arc::new(instr_ctx.feature_set.clone()),
        blockhash,
        lamports_per_signature,
    );

    // TODO: support different versions
    let sbpf_version = &SBPFVersion::V2;

    // Set up memory mapping
    let vm_ctx = input.vm_ctx.unwrap();
    let rodata = vm_ctx.rodata;
    let mut stack = vec![0; STACK_SIZE];
    let heap_max = vm_ctx.heap_max;
    let mut heap = vec![0; heap_max as usize];
    let mut regions = vec![
        MemoryRegion::new_readonly(&rodata, ebpf::MM_PROGRAM_START),
        MemoryRegion::new_writable_gapped(&mut stack, ebpf::MM_STACK_START, 0),
        MemoryRegion::new_writable(&mut heap, ebpf::MM_HEAP_START),
    ];
    let mut input_data_regions = vm_ctx.input_data_regions.clone();
    for input_data_region in &mut input_data_regions {
        if input_data_region.is_writable {
            regions.push(MemoryRegion::new_writable(
                input_data_region.content.as_mut_slice(),
                MM_INPUT_START.saturating_add(input_data_region.offset),
            ));
        } else {
            regions.push(MemoryRegion::new_readonly(
                input_data_region.content.as_slice(),
                MM_INPUT_START.saturating_add(input_data_region.offset),
            ));
        }
    }
    let config = &Config {
        aligned_memory_mapping: true,
        enable_sbpf_v2: true,
        ..Config::default()
    };
    let memory_mapping = MemoryMapping::new(regions, config, sbpf_version).unwrap();

    // Set up the vm instance
    let loader = std::sync::Arc::new(BuiltinProgram::new_mock());
    let mut vm = EbpfVm::new(
        loader,
        &SBPFVersion::V2,
        &mut invoke_context,
        memory_mapping,
        STACK_SIZE,
    );
    vm.registers[0] = vm_ctx.r0;
    vm.registers[1] = vm_ctx.r1;
    vm.registers[2] = vm_ctx.r2;
    vm.registers[3] = vm_ctx.r3;
    vm.registers[4] = vm_ctx.r4;
    vm.registers[5] = vm_ctx.r5;
    vm.registers[6] = vm_ctx.r6;
    vm.registers[7] = vm_ctx.r7;
    vm.registers[8] = vm_ctx.r8;
    vm.registers[9] = vm_ctx.r9;
    vm.registers[10] = vm_ctx.r10;
    vm.registers[11] = vm_ctx.r11;

    // Actually invoke the syscall
    let program_runtime_environment_v1 = create_program_runtime_environment_v1(
        &instr_ctx.feature_set,
        &ComputeBudget::default(),
        true,
        false,
    )
    .unwrap();

    // Invoke the syscall
    let (_, syscall_func) = program_runtime_environment_v1
        .get_function_registry()
        .lookup_by_name(&input.syscall_invocation?.function_name)?;
    syscall_func(
        &mut vm, vm_ctx.r1, vm_ctx.r2, vm_ctx.r3, vm_ctx.r4, vm_ctx.r5,
    );

    // Unwrap and return the effects of the syscall
    let program_result = vm.program_result;
    Some(SyscallEffects {
        error: match program_result {
            solana_program_runtime::solana_rbpf::error::StableResult::Ok(_) => 0,
            solana_program_runtime::solana_rbpf::error::StableResult::Err(e) => unsafe {
                let error_ptr = &e as *const EbpfError as *const u64;
                (*error_ptr).try_into().unwrap()
            },
        },
        r0: vm.registers[0],
        cu_avail: vm.context_object_pointer.get_remaining(),
        heap,
        stack,
        inputdata: input_data_regions
            .iter()
            .flat_map(|region| region.content.clone())
            .collect(),
        frame_count: vm.call_depth,
        log: invoke_context
            .get_log_collector()?
            .borrow()
            .get_recorded_content()
            .iter()
            .fold(String::new(), |mut acc, s| {
                acc.push_str(s);
                acc
            })
            .into_bytes(),
    })
}
