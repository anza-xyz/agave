//! SBF virtual machine provisioning and execution.

#[cfg(feature = "svm-internal")]
use qualifier_attr::qualifiers;
use {
    crate::{
        execution_budget::MAX_INSTRUCTION_STACK_DEPTH_SIMD_0268,
        invoke_context::{BpfAllocator, InvokeContext},
        mem_pool::VmMemoryPool,
        memory_context::{MemoryContext, SerializedAccountMetadata},
        program_cache_entry::ProgramCacheEntry,
        serialization, stable_log,
    },
    solana_instruction::error::InstructionError,
    solana_program_entrypoint::{MAX_PERMITTED_DATA_INCREASE, SUCCESS},
    solana_sbpf::{
        ebpf::{self, MM_HEAP_START, MM_STACK_START},
        elf::Executable,
        error::{EbpfError, ProgramResult},
        memory_region::{AccessType, MemoryMapping, MemoryRegion},
        vm::{ContextObject, EbpfVm, ExecutionMode},
    },
    solana_sdk_ids::bpf_loader_deprecated,
    solana_svm_log_collector::ic_logger_msg,
    solana_svm_measure::measure::Measure,
    solana_transaction_context::IndexOfAccount,
    std::{cell::RefCell, mem, time::Duration},
};

thread_local! {
    pub static MEMORY_POOL: RefCell<VmMemoryPool> = RefCell::new(VmMemoryPool::new());
}

/// Only used in macro, do not use directly!
pub fn calculate_heap_cost(heap_size: u32, heap_cost: u64) -> u64 {
    const KIBIBYTE: u64 = 1024;
    const PAGE_SIZE_KB: u64 = 32;
    let mut rounded_heap_size = u64::from(heap_size);
    rounded_heap_size =
        rounded_heap_size.saturating_add(PAGE_SIZE_KB.saturating_mul(KIBIBYTE).saturating_sub(1));
    rounded_heap_size
        .checked_div(PAGE_SIZE_KB.saturating_mul(KIBIBYTE))
        .expect("PAGE_SIZE_KB * KIBIBYTE > 0")
        .saturating_sub(1)
        .saturating_mul(heap_cost)
}

/// Only used in macro, do not use directly!
///
/// # Safety
///
/// Refer to [`configure_program_regions`].
#[cfg_attr(feature = "svm-internal", qualifiers(pub))]
pub unsafe fn create_vm<'a, 'b>(
    program: &'a Executable<InvokeContext<'b, 'b>>,
    invoke_context: &'a mut InvokeContext<'b, 'b>,
    stack: *mut [u8],
    heap: *mut [u8],
) -> Result<EbpfVm<'a, InvokeContext<'b, 'b>>, Box<dyn std::error::Error>> {
    let stack_size = stack.len();
    unsafe {
        // SAFETY: invariants delegated to the caller.
        configure_program_regions(invoke_context, program, stack, heap)?;
    }
    Ok(EbpfVm::new(
        program.get_loader().clone(),
        program.get_sbpf_version(),
        invoke_context,
        stack_size,
    ))
}

/// # Safety
///
/// The `executable`, `stack` and `heap` arguments must remain allocated for at least the lifetime
/// of [`MemoryMapping`] (or until after the `MemoryMapping` is reconfigured with different
/// `executable`, `stack` and `heap`).
unsafe fn configure_program_regions<C: ContextObject>(
    invoke_context: &mut InvokeContext,
    executable: &Executable<C>,
    stack: *mut [u8],
    heap: *mut [u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let mapping = invoke_context.memory_contexts.memory_mapping_mut()?;
    let regions = mapping.get_regions_mut();
    let [ro_area, stack_area, heap_area, ..] = regions else {
        panic!("the regions vector must have at least three entries")
    };
    *ro_area = executable.get_ro_region();
    let sbpf_version = executable.get_sbpf_version();
    let config = executable.get_config();
    *stack_area = MemoryRegion::new_gapped(
        stack,
        MM_STACK_START,
        if sbpf_version.stack_frame_gaps() && config.enable_stack_frame_gaps {
            config.stack_frame_size as u64
        } else {
            0
        },
    );
    *heap_area = MemoryRegion::new(heap, MM_HEAP_START);
    mapping
        .initialize()
        .map_err(|err| Box::new(err) as Box<dyn std::error::Error>)
}

/// Create the SBF virtual machine
#[macro_export]
macro_rules! create_vm {
    ($vm:ident, $program:expr, $invoke_context:expr $(,)?) => {
        let invoke_context = &*$invoke_context;
        let stack_size = $program.get_config().stack_size();
        let heap_size = invoke_context.get_compute_budget().heap_size;
        let heap_cost_result =
            invoke_context
                .compute_meter
                .consume_checked($crate::__private::calculate_heap_cost(
                    heap_size,
                    invoke_context.get_execution_cost().heap_cost,
                ));
        let $vm = heap_cost_result.and_then(|_| {
            let (mut stack, mut heap) = $crate::__private::MEMORY_POOL
                .with_borrow_mut(|pool| (pool.get_stack(stack_size), pool.get_heap(heap_size)));
            let vm = $crate::__private::create_vm(
                $program,
                $invoke_context,
                stack
                    .as_slice_mut()
                    .get_mut(..stack_size)
                    .expect("invalid stack size"),
                heap.as_slice_mut()
                    .get_mut(..heap_size as usize)
                    .expect("invalid heap size"),
            );
            vm.map(|vm| (vm, stack, heap))
        });
    };
}

/// # Safety
///
/// The [`MemoryRegion`]s must satisfy the safety preconditions for
/// [`MemoryMapping::new_uninitialized`].
unsafe fn set_memory_context<'b>(
    additional_initialized_regions: Vec<MemoryRegion>,
    accounts_metadata: Vec<SerializedAccountMetadata>,
    invoke_context: &mut InvokeContext<'b, 'b>,
    executable: &Executable<InvokeContext<'b, 'b>>,
    virtual_address_space_adjustments: bool,
    account_data_direct_mapping: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let heap_size = invoke_context.get_compute_budget().heap_size;
    let regions = vec![MemoryRegion::default(); 3]
        .into_iter()
        .chain(additional_initialized_regions)
        .collect();
    let memory_mapping = unsafe {
        // SAFETY: all memory regions are `default` (and thus implicitly valid) or valid by
        // delegating the safety invariant upon the caller.
        MemoryMapping::new_uninitialized(
            regions,
            executable.get_config(),
            executable.get_sbpf_version(),
            invoke_context.transaction_context.access_violation_handler(
                virtual_address_space_adjustments,
                account_data_direct_mapping,
            ),
        )
    };

    invoke_context
        .memory_contexts
        .set_memory_context(MemoryContext::new(
            BpfAllocator::new(heap_size as u64),
            accounts_metadata,
            memory_mapping,
        ))
        .map_err(|err| Box::new(err) as Box<dyn std::error::Error>)
}

#[cfg_attr(feature = "svm-internal", qualifiers(pub))]
pub fn execute<'a, 'b: 'a>(
    executable: &'a Executable<InvokeContext<'static, 'static>>,
    invoke_context: &'a mut InvokeContext<'b, 'b>,
    cache_entry: &ProgramCacheEntry,
) -> Result<(), Box<dyn std::error::Error>> {
    // We dropped the lifetime tracking in the Executor by setting it to 'static,
    // thus we need to reintroduce the correct lifetime of InvokeContext here again.
    let executable = unsafe {
        mem::transmute::<
            &'a Executable<InvokeContext<'static, 'static>>,
            &'a Executable<InvokeContext<'b, 'b>>,
        >(executable)
    };
    let log_collector = invoke_context.get_log_collector();
    let transaction_context = &invoke_context.transaction_context;
    let instruction_context = transaction_context.get_current_instruction_context()?;
    let program_id = *instruction_context.get_program_key()?;
    let is_loader_deprecated =
        instruction_context.get_program_owner()? == bpf_loader_deprecated::id();
    let virtual_address_space_adjustments = invoke_context
        .get_feature_set()
        .virtual_address_space_adjustments;
    let account_data_direct_mapping = invoke_context.get_feature_set().account_data_direct_mapping;
    let direct_account_pointers_in_program_input = invoke_context
        .get_feature_set()
        .direct_account_pointers_in_program_input;

    let mut serialize_time = Measure::start("serialize");
    let (parameter_bytes, regions, accounts_metadata, instruction_data_offset) =
        serialization::serialize_parameters(
            &instruction_context,
            virtual_address_space_adjustments,
            account_data_direct_mapping,
            direct_account_pointers_in_program_input,
        )?;
    serialize_time.stop();

    // save the account addresses so in case we hit an AccessViolation error we
    // can map to a more specific error
    let account_region_addrs = accounts_metadata
        .iter()
        .map(|m| {
            let vm_end = m
                .vm_data_addr
                .saturating_add(m.original_data_len as u64)
                .saturating_add(if !is_loader_deprecated {
                    MAX_PERMITTED_DATA_INCREASE as u64
                } else {
                    0
                });
            m.vm_data_addr..vm_end
        })
        .collect::<Vec<_>>();

    #[cfg(feature = "sbpf-debugger")]
    let (debug_port, debug_metadata) = if invoke_context.debug_port.is_some() {
        (
            invoke_context.debug_port,
            Some(format!(
                "program_id={};cpi_level={};caller={}",
                program_id,
                instruction_context.get_stack_height().saturating_sub(1),
                invoke_context
                    .get_stack_height()
                    .checked_sub(2)
                    .and_then(|nesting_level| {
                        transaction_context
                            .get_instruction_context_at_nesting_level(nesting_level)
                            .ok()
                    })
                    .and_then(|ctx| ctx.get_program_key().ok())
                    .map(|key| key.to_string())
                    .unwrap_or_else(|| "none".into())
            )),
        )
    } else {
        (None, None)
    };

    let mut create_vm_time = Measure::start("create_vm");
    unsafe {
        // SAFETY: The memory pointed to by regions is valid for the useful lifetime of
        // `invoke_context`, which in turn contains the `MemoryMapping` that allows access to this
        // memory.
        set_memory_context(
            regions,
            accounts_metadata,
            invoke_context,
            executable,
            virtual_address_space_adjustments,
            account_data_direct_mapping,
        )?
    };

    let execution_result = {
        let mut execution_mode = ExecutionMode::PreferJit;

        #[cfg(feature = "sbpf-debugger")]
        if invoke_context.debug_port.is_some() {
            execution_mode = ExecutionMode::Interpreted;
        }

        let compute_meter_prev = invoke_context.get_remaining();
        let (mut vm, stack, heap) = unsafe {
            // SAFETY: The `stack`, `heap` and `executable` live past the lifetime of
            // `invoke_context`.
            create_vm!(vm, executable, invoke_context);
            match vm {
                Ok(info) => info,
                Err(e) => {
                    ic_logger_msg!(log_collector, "Failed to create SBF VM: {}", e);
                    return Err(Box::new(InstructionError::ProgramEnvironmentSetupFailure));
                }
            }
        };

        create_vm_time.stop();
        #[cfg(feature = "sbpf-debugger")]
        {
            vm.debug_port = debug_port;
            vm.debug_metadata = debug_metadata;
        }

        let execute_time = Measure::start("execute");
        let prev_nested_exec_time = vm.context().total_nested_exec_time;

        vm.registers[1] = ebpf::MM_INPUT_START;
        vm.registers[2] = instruction_data_offset as u64;
        let mut call_frames =
            MEMORY_POOL.with_borrow_mut(|memory_pool| memory_pool.get_call_frames());
        let (compute_units_consumed, result) =
            vm.execute_program(executable, &mut execution_mode, &mut call_frames);
        let register_trace = std::mem::take(&mut vm.register_trace);
        MEMORY_POOL.with_borrow_mut(|memory_pool| {
            memory_pool.put_stack(stack);
            memory_pool.put_heap(heap);
            memory_pool.put_call_frames(call_frames);
            debug_assert!(memory_pool.stack_len() <= MAX_INSTRUCTION_STACK_DEPTH_SIMD_0268);
            debug_assert!(memory_pool.heap_len() <= MAX_INSTRUCTION_STACK_DEPTH_SIMD_0268);
        });
        drop(vm);
        invoke_context.insert_register_trace(register_trace);

        // This section is a little convoluted due to the nested and sibling (CPI) invocations.
        let total_execute_ns = execute_time.end_as_ns();
        let nested_execution_time_delta = invoke_context
            .total_nested_exec_time
            .saturating_sub(prev_nested_exec_time);
        let this_call_ns =
            total_execute_ns.saturating_sub(nested_execution_time_delta.as_nanos() as u64);
        invoke_context.total_nested_exec_time = invoke_context
            .total_nested_exec_time
            .saturating_add(Duration::from_nanos(this_call_ns));
        let this_call_us = this_call_ns / 1000;
        invoke_context.timings.execute_us += this_call_us;
        match execution_mode {
            ExecutionMode::Interpreted => cache_entry.stats.interpreter_executed(this_call_us),
            ExecutionMode::Jit => cache_entry.stats.jit_executed(this_call_us),
            ExecutionMode::PreferJit => { /* not actually executed? */ }
        }

        ic_logger_msg!(
            log_collector,
            "Program {} consumed {} of {} compute units",
            &program_id,
            compute_units_consumed,
            compute_meter_prev
        );
        let (_returned_from_program_id, return_data) =
            invoke_context.transaction_context.get_return_data();
        if !return_data.is_empty() {
            stable_log::program_return(&log_collector, &program_id, return_data);
        }
        match result {
            ProgramResult::Ok(status) if status != SUCCESS => {
                let error: InstructionError = status.into();
                Err(Box::new(error) as Box<dyn std::error::Error>)
            }
            ProgramResult::Err(mut error) => {
                // Don't clean me up!!
                // This feature is active on all networks, but we still toggle
                // it off during fuzzing.
                if invoke_context
                    .get_feature_set()
                    .deplete_cu_meter_on_vm_failure
                    && !matches!(error, EbpfError::SyscallError(_))
                {
                    // when an exception is thrown during the execution of a
                    // Basic Block (e.g., a null memory dereference or other
                    // faults), determining the exact number of CUs consumed
                    // up to the point of failure requires additional effort
                    // and is unnecessary since these cases are rare.
                    //
                    // In order to simplify CU tracking, simply consume all
                    // remaining compute units so that the block cost
                    // tracker uses the full requested compute unit cost for
                    // this failed transaction.
                    invoke_context.consume(invoke_context.get_remaining());
                }

                if virtual_address_space_adjustments {
                    if let EbpfError::SyscallError(err) = error {
                        error = err
                            .downcast::<EbpfError>()
                            .map(|err| *err)
                            .unwrap_or_else(EbpfError::SyscallError);
                    }
                    if let EbpfError::AccessViolation(access_type, vm_addr, len, _section_name) =
                        error
                    {
                        // If virtual_address_space_adjustments is enabled and a program tries to write to a readonly
                        // region we'll get a memory access violation. Map it to a more specific
                        // error so it's easier for developers to see what happened.
                        if let Some((instruction_account_index, vm_addr_range)) =
                            account_region_addrs
                                .iter()
                                .enumerate()
                                .find(|(_, vm_addr_range)| vm_addr_range.contains(&vm_addr))
                        {
                            let transaction_context = &invoke_context.transaction_context;
                            let instruction_context =
                                transaction_context.get_current_instruction_context()?;
                            let account = instruction_context.try_borrow_instruction_account(
                                instruction_account_index as IndexOfAccount,
                            )?;
                            if vm_addr.saturating_add(len) <= vm_addr_range.end {
                                // The access was within the range of the accounts address space,
                                // but it might not be within the range of the actual data.
                                let is_access_outside_of_data = vm_addr
                                    .saturating_add(len)
                                    .saturating_sub(vm_addr_range.start)
                                    as usize
                                    > account.get_data().len();
                                error = EbpfError::SyscallError(Box::new(match access_type {
                                    AccessType::Store => {
                                        if let Err(err) = account.can_data_be_changed() {
                                            err
                                        } else {
                                            // The store was allowed but failed,
                                            // thus it must have been an attempt to grow the account.
                                            debug_assert!(is_access_outside_of_data);
                                            InstructionError::InvalidRealloc
                                        }
                                    }
                                    AccessType::Load => {
                                        // Loads should only fail when they are outside of the account data.
                                        debug_assert!(is_access_outside_of_data);
                                        if account.can_data_be_changed().is_err() {
                                            // Load beyond readonly account data happened because the program
                                            // expected more data than there actually is.
                                            InstructionError::AccountDataTooSmall
                                        } else {
                                            // Load beyond writable account data also attempted to grow.
                                            InstructionError::InvalidRealloc
                                        }
                                    }
                                }));
                            }
                        }
                    }
                }
                Err(if let EbpfError::SyscallError(err) = error {
                    err
                } else {
                    error.into()
                })
            }
            _ => Ok(()),
        }
    };

    fn deserialize_parameters(
        invoke_context: &mut InvokeContext,
        parameter_bytes: &[u8],
        virtual_address_space_adjustments: bool,
        account_data_direct_mapping: bool,
    ) -> Result<(), InstructionError> {
        serialization::deserialize_parameters(
            &invoke_context
                .transaction_context
                .get_current_instruction_context()?,
            virtual_address_space_adjustments,
            account_data_direct_mapping,
            parameter_bytes,
            &invoke_context
                .memory_contexts
                .memory_context()?
                .accounts_metadata,
        )
    }

    let mut deserialize_time = Measure::start("deserialize");
    let execute_or_deserialize_result = execution_result.and_then(|_| {
        deserialize_parameters(
            invoke_context,
            parameter_bytes.as_slice(),
            virtual_address_space_adjustments,
            account_data_direct_mapping,
        )
        .map_err(|error| Box::new(error) as Box<dyn std::error::Error>)
    });
    deserialize_time.stop();

    // Update the timings
    invoke_context.timings.serialize_us += serialize_time.as_us();
    invoke_context.timings.create_vm_us += create_vm_time.as_us();
    invoke_context.timings.deserialize_us += deserialize_time.as_us();

    execute_or_deserialize_result
}
