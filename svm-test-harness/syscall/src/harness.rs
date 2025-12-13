//! Solana SVM test harness for syscalls.
//!
//! This module provides an API for testing individual syscall implementations
//! against the VM.

use {
    crate::vm_utils::{err_map::unpack_stable_result, mem_regions, HEAP_MAX, STACK_SIZE},
    agave_feature_set::FeatureSet,
    agave_precompiles::{get_precompile, is_precompile},
    agave_syscalls::create_program_runtime_environment_v1,
    solana_account::AccountSharedData,
    solana_compute_budget::compute_budget::{ComputeBudget, SVMTransactionExecutionCost},
    solana_instruction::AccountMeta,
    solana_precompile_error::PrecompileError,
    solana_program_runtime::{
        invoke_context::{EnvironmentConfig, InvokeContext},
        loaded_programs::{ProgramCacheForTxBatch, ProgramRuntimeEnvironments},
        mem_pool::VmMemoryPool,
        serialization::serialize_parameters,
        sysvar_cache::SysvarCache,
    },
    solana_pubkey::Pubkey,
    solana_sbpf::{
        aligned_memory::AlignedMemory,
        ebpf::{self, HOST_ALIGN},
        memory_region::{MemoryMapping, MemoryRegion},
        program::{BuiltinProgram, SBPFVersion},
        vm::{ContextObject, EbpfVm},
    },
    solana_stable_layout::stable_vec::StableVec,
    solana_svm_callback::InvokeContextCallback,
    solana_svm_feature_set::SVMFeatureSet,
    solana_svm_log_collector::LogCollector,
    solana_svm_test_harness_fixture::{
        instr_context::InstrContext, syscall_context::SyscallContext,
        syscall_effects::SyscallEffects,
    },
    solana_svm_test_harness_instr::{program_cache, sysvar_cache},
    solana_transaction_context::{
        instruction_accounts::InstructionAccount, transaction_accounts::KeyedAccountSharedData,
        IndexOfAccount, TransactionContext,
    },
    std::sync::Arc,
};

struct SyscallContextCallback {
    feature_set: FeatureSet,
}

impl SyscallContextCallback {
    fn new(feature_set: FeatureSet) -> Self {
        Self { feature_set }
    }
}

impl InvokeContextCallback for SyscallContextCallback {
    fn is_precompile(&self, program_id: &Pubkey) -> bool {
        is_precompile(program_id, |feature_id: &Pubkey| {
            self.feature_set.is_active(feature_id)
        })
    }

    fn process_precompile(
        &self,
        program_id: &Pubkey,
        data: &[u8],
        instruction_datas: Vec<&[u8]>,
    ) -> std::result::Result<(), PrecompileError> {
        if let Some(precompile) = get_precompile(program_id, |feature_id: &Pubkey| {
            self.feature_set.is_active(feature_id)
        }) {
            precompile.verify(data, &instruction_datas, &self.feature_set)
        } else {
            Err(PrecompileError::InvalidPublicKey)
        }
    }
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

/// Cleanup leaked static pointers.
fn cleanup_static_ptrs(
    transaction_context_ptr: usize,
    sysvar_cache_ptr: usize,
    program_cache_ptr: usize,
    runtime_features_ptr: usize,
    instr_ctx_ptr: usize,
    callback_ptr: usize,
    environments_ptr: usize,
) {
    unsafe {
        let _ = Box::from_raw(transaction_context_ptr as *mut TransactionContext);
        let _ = Box::from_raw(sysvar_cache_ptr as *mut SysvarCache);
        let _ = Box::from_raw(program_cache_ptr as *mut ProgramCacheForTxBatch);
        let _ = Box::from_raw(runtime_features_ptr as *mut SVMFeatureSet);
        let _ = Box::from_raw(instr_ctx_ptr as *mut InstrContext);
        let _ = Box::from_raw(callback_ptr as *mut SyscallContextCallback);
        let _ = Box::from_raw(environments_ptr as *mut ProgramRuntimeEnvironments);
    }
}

/// Execute a single syscall against the Solana VM.
pub fn execute_vm_syscall(input: SyscallContext) -> Option<SyscallEffects> {
    let instr_ctx = input.instr_ctx;
    let runtime_feature_set = instr_ctx.feature_set.runtime_features();
    let feature_set_snapshot = instr_ctx.feature_set.clone();

    let program_id = instr_ctx.instruction.program_id;
    let instruction_data = instr_ctx.instruction.data.to_vec();
    let instruction_accounts_snapshot: StableVec<AccountMeta> = instr_ctx
        .instruction
        .accounts
        .iter()
        .cloned()
        .collect::<Vec<_>>()
        .into();

    let simd_0268_active = instr_ctx
        .feature_set
        .is_active(&agave_feature_set::raise_cpi_nesting_limit_to_8::id());
    let simd_0339_active = instr_ctx
        .feature_set
        .is_active(&agave_feature_set::increase_cpi_account_info_limit::id());

    let compute_budget = {
        let mut budget = ComputeBudget::new_with_defaults(simd_0268_active, simd_0339_active);
        budget.compute_unit_limit = instr_ctx.cu_avail;
        budget
    };

    let mut sysvar_cache = SysvarCache::default();
    sysvar_cache::fill_from_accounts(&mut sysvar_cache, &instr_ctx.accounts);

    let clock = sysvar_cache.get_clock().ok()?;
    let rent = sysvar_cache.get_rent().ok()?;

    #[allow(deprecated)]
    let (blockhash, lamports_per_signature) = sysvar_cache
        .get_recent_blockhashes()
        .ok()
        .and_then(|x| (*x).last().cloned())
        .map(|x| (x.blockhash, x.fee_calculator.lamports_per_signature))
        .unwrap_or_default();

    let mut transaction_accounts: Vec<KeyedAccountSharedData> = instr_ctx
        .accounts
        .iter()
        .map(|(pubkey, account)| (*pubkey, AccountSharedData::from(account.clone())))
        .collect();

    if !transaction_accounts
        .iter()
        .any(|(pubkey, _)| pubkey == &program_id)
    {
        transaction_accounts.push((program_id, AccountSharedData::default()));
    }

    let transaction_context = TransactionContext::new(
        transaction_accounts,
        (*rent).clone(),
        compute_budget.max_instruction_stack_depth,
        compute_budget.max_instruction_trace_length,
    );

    // Leak everything to create 'static references
    let instr_ctx = Box::leak(Box::new(instr_ctx));
    let instr_ctx_ptr = instr_ctx as *mut InstrContext as usize;

    let transaction_context = Box::leak(Box::new(transaction_context));
    let transaction_context_ptr = transaction_context as *mut TransactionContext as usize;

    let sysvar_cache = Box::leak(Box::new(sysvar_cache));
    let sysvar_cache_ptr = sysvar_cache as *mut SysvarCache as usize;

    let runtime_features = Box::leak(Box::new(runtime_feature_set));
    let runtime_features_ptr = runtime_features as *mut SVMFeatureSet as usize;

    if let Some(return_data) = input.vm_ctx.return_data.clone() {
        let _ = transaction_context.set_return_data(return_data.program_id, return_data.data);
    }

    let log_collector = LogCollector::new_ref();
    let instr_accounts = get_instr_accounts(transaction_context, &instruction_accounts_snapshot);

    let Ok(program_runtime_environment_v1) = create_program_runtime_environment_v1(
        runtime_features,
        &compute_budget.to_budget(),
        false,
        std::env::var("ENABLE_VM_TRACING").is_ok(),
    ) else {
        // Partial cleanup - we haven't created all pointers yet
        unsafe {
            let _ = Box::from_raw(transaction_context_ptr as *mut TransactionContext);
            let _ = Box::from_raw(sysvar_cache_ptr as *mut SysvarCache);
            let _ = Box::from_raw(runtime_features_ptr as *mut SVMFeatureSet);
            let _ = Box::from_raw(instr_ctx_ptr as *mut InstrContext);
        }
        return None;
    };
    let environments = ProgramRuntimeEnvironments {
        program_runtime_v1: Arc::new(program_runtime_environment_v1),
        ..ProgramRuntimeEnvironments::default()
    };
    let environments = Box::leak(Box::new(environments));
    let environments_ptr = environments as *mut ProgramRuntimeEnvironments as usize;

    let mut program_cache = ProgramCacheForTxBatch::default();
    program_cache.set_slot_for_tests(clock.slot);
    if program_cache::fill_from_accounts(
        &mut program_cache,
        environments,
        &instr_ctx.accounts,
        clock.slot,
    )
    .is_err()
    {
        unsafe {
            let _ = Box::from_raw(transaction_context_ptr as *mut TransactionContext);
            let _ = Box::from_raw(sysvar_cache_ptr as *mut SysvarCache);
            let _ = Box::from_raw(runtime_features_ptr as *mut SVMFeatureSet);
            let _ = Box::from_raw(instr_ctx_ptr as *mut InstrContext);
            let _ = Box::from_raw(environments_ptr as *mut ProgramRuntimeEnvironments);
        }
        return None;
    }

    let program_cache = Box::leak(Box::new(program_cache));
    let program_cache_ptr = program_cache as *mut ProgramCacheForTxBatch as usize;

    let callback = Box::leak(Box::new(SyscallContextCallback::new(feature_set_snapshot)));
    let callback_ptr = callback as *mut SyscallContextCallback as usize;

    let mut invoke_ctx = InvokeContext::new(
        transaction_context,
        program_cache,
        EnvironmentConfig::new(
            blockhash,
            lamports_per_signature,
            callback,
            runtime_features,
            environments,
            environments,
            sysvar_cache,
        ),
        Some(log_collector.clone()),
        compute_budget.to_budget(),
        SVMTransactionExecutionCost::default(),
    );

    let Some(program_idx) = invoke_ctx
        .transaction_context
        .find_index_of_account(&program_id)
    else {
        cleanup_static_ptrs(
            transaction_context_ptr,
            sysvar_cache_ptr,
            program_cache_ptr,
            runtime_features_ptr,
            instr_ctx_ptr,
            callback_ptr,
            environments_ptr,
        );
        return None;
    };
    if program_idx > 255 {
        cleanup_static_ptrs(
            transaction_context_ptr,
            sysvar_cache_ptr,
            program_cache_ptr,
            runtime_features_ptr,
            instr_ctx_ptr,
            callback_ptr,
            environments_ptr,
        );
        return None;
    }

    let direct_mapping = invoke_ctx.get_feature_set().account_data_direct_mapping;
    let stricter_abi_and_runtime_constraints = invoke_ctx
        .get_feature_set()
        .stricter_abi_and_runtime_constraints;
    let mask_out_rent_epoch_in_vm_serialization = invoke_ctx
        .get_feature_set()
        .mask_out_rent_epoch_in_vm_serialization;

    if invoke_ctx
        .transaction_context
        .configure_next_instruction_for_tests(program_idx, instr_accounts, instruction_data)
        .is_err()
    {
        cleanup_static_ptrs(
            transaction_context_ptr,
            sysvar_cache_ptr,
            program_cache_ptr,
            runtime_features_ptr,
            instr_ctx_ptr,
            callback_ptr,
            environments_ptr,
        );
        return None;
    }

    if invoke_ctx.push().is_err() {
        cleanup_static_ptrs(
            transaction_context_ptr,
            sysvar_cache_ptr,
            program_cache_ptr,
            runtime_features_ptr,
            instr_ctx_ptr,
            callback_ptr,
            environments_ptr,
        );
        return None;
    }

    let Ok(caller_instr_ctx) = invoke_ctx
        .transaction_context
        .get_current_instruction_context()
    else {
        cleanup_static_ptrs(
            transaction_context_ptr,
            sysvar_cache_ptr,
            program_cache_ptr,
            runtime_features_ptr,
            instr_ctx_ptr,
            callback_ptr,
            environments_ptr,
        );
        return None;
    };

    let Ok((_aligned_memory, input_memory_regions, acc_metadatas, _instruction_data_offset)) =
        serialize_parameters(
            &caller_instr_ctx,
            stricter_abi_and_runtime_constraints,
            direct_mapping,
            mask_out_rent_epoch_in_vm_serialization,
        )
    else {
        cleanup_static_ptrs(
            transaction_context_ptr,
            sysvar_cache_ptr,
            program_cache_ptr,
            runtime_features_ptr,
            instr_ctx_ptr,
            callback_ptr,
            environments_ptr,
        );
        return None;
    };

    let sbpf_version = SBPFVersion::V0;
    let vm_ctx = &input.vm_ctx;

    if vm_ctx.heap_max as usize > HEAP_MAX {
        cleanup_static_ptrs(
            transaction_context_ptr,
            sysvar_cache_ptr,
            program_cache_ptr,
            runtime_features_ptr,
            instr_ctx_ptr,
            callback_ptr,
            environments_ptr,
        );
        return None;
    }

    let config = environments.program_runtime_v1.get_config().clone();
    let Some((_, syscall_func)) = environments
        .program_runtime_v1
        .get_function_registry()
        .lookup_by_name(&input.syscall_invocation.function_name)
    else {
        cleanup_static_ptrs(
            transaction_context_ptr,
            sysvar_cache_ptr,
            program_cache_ptr,
            runtime_features_ptr,
            instr_ctx_ptr,
            callback_ptr,
            environments_ptr,
        );
        return None;
    };

    let mut mempool = VmMemoryPool::new();
    let rodata = AlignedMemory::<HOST_ALIGN>::from(&vm_ctx.rodata);
    let mut stack = mempool.get_stack(STACK_SIZE);
    let mut heap = AlignedMemory::<HOST_ALIGN>::from(&vec![0; vm_ctx.heap_max as usize]);

    let rodata_stack_heap = vec![
        MemoryRegion::new_readonly(rodata.as_slice(), ebpf::MM_RODATA_START),
        MemoryRegion::new_writable_gapped(
            stack.as_slice_mut(),
            ebpf::MM_STACK_START,
            if !sbpf_version.dynamic_stack_frames() && config.enable_stack_frame_gaps {
                config.stack_frame_size as u64
            } else {
                0
            },
        ),
        MemoryRegion::new_writable(heap.as_slice_mut(), ebpf::MM_HEAP_START),
    ];
    let regions = rodata_stack_heap
        .into_iter()
        .chain(input_memory_regions)
        .collect();

    let Ok(memory_mapping) = MemoryMapping::new_with_access_violation_handler(
        regions,
        &config,
        sbpf_version,
        invoke_ctx
            .transaction_context
            .access_violation_handler(stricter_abi_and_runtime_constraints, direct_mapping),
    ) else {
        cleanup_static_ptrs(
            transaction_context_ptr,
            sysvar_cache_ptr,
            program_cache_ptr,
            runtime_features_ptr,
            instr_ctx_ptr,
            callback_ptr,
            environments_ptr,
        );
        return None;
    };

    if invoke_ctx
        .set_syscall_context(solana_program_runtime::invoke_context::SyscallContext {
            allocator: solana_program_runtime::invoke_context::BpfAllocator::new(vm_ctx.heap_max),
            accounts_metadata: acc_metadatas,
        })
        .is_err()
    {
        cleanup_static_ptrs(
            transaction_context_ptr,
            sysvar_cache_ptr,
            program_cache_ptr,
            runtime_features_ptr,
            instr_ctx_ptr,
            callback_ptr,
            environments_ptr,
        );
        return None;
    }

    let loader = Arc::new(BuiltinProgram::new_loader(config.clone()));
    let mut vm = EbpfVm::new(
        loader,
        sbpf_version,
        &mut invoke_ctx,
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

    mem_regions::copy_memory_prefix(heap.as_slice_mut(), &input.syscall_invocation.heap_prefix);
    mem_regions::copy_memory_prefix(stack.as_slice_mut(), &input.syscall_invocation.stack_prefix);

    vm.invoke_function(syscall_func);

    let program_result = vm.program_result;
    let (error, error_kind, r0) =
        unpack_stable_result(program_result, vm.context_object_pointer, &program_id);

    let effects = SyscallEffects {
        r0,
        r1: 0,
        r2: 0,
        r3: 0,
        r4: 0,
        r5: 0,
        r6: 0,
        r7: 0,
        r8: 0,
        r9: 0,
        r10: 0,
        cu_avail: vm.context_object_pointer.get_remaining(),
        heap: heap.as_slice().into(),
        stack: stack.as_slice().into(),
        input_data_regions: mem_regions::extract_input_data_regions(&vm.memory_mapping),
        inputdata: vec![],
        rodata: rodata.as_slice().into(),
        frame_count: vm.call_depth,
        error,
        error_kind,
        log: invoke_ctx
            .get_log_collector()
            .map(|lc| lc.borrow().get_recorded_content().join("\n").into_bytes())
            .unwrap_or_default(),
        pc: 0,
    };

    cleanup_static_ptrs(
        transaction_context_ptr,
        sysvar_cache_ptr,
        program_cache_ptr,
        runtime_features_ptr,
        instr_ctx_ptr,
        callback_ptr,
        environments_ptr,
    );

    Some(effects)
}
