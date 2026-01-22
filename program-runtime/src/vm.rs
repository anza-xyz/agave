//! SBF virtual machine provisioning and execution.

#[cfg(feature = "svm-internal")]
use qualifier_attr::qualifiers;
use {
    crate::{
        execution_budget::MAX_INSTRUCTION_STACK_DEPTH,
        invoke_context::{BpfAllocator, InvokeContext, SerializedAccountMetadata, SyscallContext},
        mem_pool::VmMemoryPool,
        serialization, stable_log,
    },
    cfg_if::cfg_if,
    solana_instruction::error::InstructionError,
    solana_program_entrypoint::{MAX_PERMITTED_DATA_INCREASE, SUCCESS},
    solana_sbpf::{
        ebpf::{self, MM_HEAP_START},
        elf::Executable,
        error::{EbpfError, ProgramResult},
        memory_region::{AccessType, MemoryMapping, MemoryRegion},
        vm::{ContextObject, EbpfVm},
    },
    solana_sdk_ids::bpf_loader_deprecated,
    solana_svm_log_collector::ic_logger_msg,
    solana_svm_measure::measure::Measure,
    solana_transaction_context::{IndexOfAccount, TransactionContext},
    std::{cell::RefCell, mem},
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
#[cfg_attr(feature = "svm-internal", qualifiers(pub))]
pub fn create_vm<'a, 'b>(
    program: &'a Executable<InvokeContext<'b, 'b>>,
    regions: Vec<MemoryRegion>,
    accounts_metadata: Vec<SerializedAccountMetadata>,
    invoke_context: &'a mut InvokeContext<'b, 'b>,
    stack: &mut [u8],
    heap: &mut [u8],
) -> Result<EbpfVm<'a, InvokeContext<'b, 'b>>, Box<dyn std::error::Error>> {
    let stack_size = stack.len();
    let heap_size = heap.len();
    let memory_mapping = create_memory_mapping(
        program,
        stack,
        heap,
        regions,
        invoke_context.transaction_context,
        invoke_context
            .get_feature_set()
            .stricter_abi_and_runtime_constraints,
        invoke_context.get_feature_set().account_data_direct_mapping,
    )?;
    invoke_context.set_syscall_context(SyscallContext {
        allocator: BpfAllocator::new(heap_size as u64),
        accounts_metadata,
    })?;
    Ok(EbpfVm::new(
        program.get_loader().clone(),
        program.get_sbpf_version(),
        invoke_context,
        memory_mapping,
        stack_size,
    ))
}

fn create_memory_mapping<'a, C: ContextObject>(
    executable: &Executable<C>,
    stack: &'a mut [u8],
    heap: &'a mut [u8],
    additional_regions: Vec<MemoryRegion>,
    transaction_context: &TransactionContext,
    stricter_abi_and_runtime_constraints: bool,
    account_data_direct_mapping: bool,
) -> Result<MemoryMapping, Box<dyn std::error::Error>> {
    let config = executable.get_config();
    let sbpf_version = executable.get_sbpf_version();
    let regions: Vec<MemoryRegion> = vec![
        executable.get_ro_region(),
        MemoryRegion::new_writable_gapped(
            stack,
            ebpf::MM_STACK_START,
            if !sbpf_version.manual_stack_frame_bump() && config.enable_stack_frame_gaps {
                config.stack_frame_size as u64
            } else {
                0
            },
        ),
        MemoryRegion::new_writable(heap, MM_HEAP_START),
    ]
    .into_iter()
    .chain(additional_regions)
    .collect();

    Ok(MemoryMapping::new_with_access_violation_handler(
        regions,
        config,
        sbpf_version,
        transaction_context.access_violation_handler(
            stricter_abi_and_runtime_constraints,
            account_data_direct_mapping,
        ),
    )?)
}

/// Create the SBF virtual machine
#[macro_export]
macro_rules! create_vm {
    ($vm:ident, $program:expr, $regions:expr, $accounts_metadata:expr, $invoke_context:expr $(,)?) => {
        let invoke_context = &*$invoke_context;
        let stack_size = $program.get_config().stack_size();
        let heap_size = invoke_context.get_compute_budget().heap_size;
        let heap_cost_result =
            invoke_context.consume_checked($crate::__private::calculate_heap_cost(
                heap_size,
                invoke_context.get_execution_cost().heap_cost,
            ));
        let $vm = heap_cost_result.and_then(|_| {
            let (mut stack, mut heap) = $crate::__private::MEMORY_POOL
                .with_borrow_mut(|pool| (pool.get_stack(stack_size), pool.get_heap(heap_size)));
            let vm = $crate::__private::create_vm(
                $program,
                $regions,
                $accounts_metadata,
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

#[cfg_attr(feature = "svm-internal", qualifiers(pub))]
pub fn execute<'a, 'b: 'a>(
    executable: &'a Executable<InvokeContext<'static, 'static>>,
    invoke_context: &'a mut InvokeContext<'b, 'b>,
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
    cfg_if! {
        if #[cfg(any(
            target_os = "windows",
            not(target_arch = "x86_64"),
            feature = "sbpf-debugger"
        ))] {
            let use_jit = false;
        } else {
            let use_jit = executable.get_compiled_program().is_some();
        }
    }
    let stricter_abi_and_runtime_constraints = invoke_context
        .get_feature_set()
        .stricter_abi_and_runtime_constraints;
    let account_data_direct_mapping = invoke_context.get_feature_set().account_data_direct_mapping;
    let mask_out_rent_epoch_in_vm_serialization = invoke_context
        .get_feature_set()
        .mask_out_rent_epoch_in_vm_serialization;
    let provide_instruction_data_offset_in_vm_r2 = invoke_context
        .get_feature_set()
        .provide_instruction_data_offset_in_vm_r2;

    let mut serialize_time = Measure::start("serialize");
    let (parameter_bytes, regions, accounts_metadata, instruction_data_offset) =
        serialization::serialize_parameters(
            &instruction_context,
            stricter_abi_and_runtime_constraints,
            account_data_direct_mapping,
            mask_out_rent_epoch_in_vm_serialization,
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

    let mut create_vm_time = Measure::start("create_vm");
    let execution_result = {
        let compute_meter_prev = invoke_context.get_remaining();
        create_vm!(vm, executable, regions, accounts_metadata, invoke_context);
        let (mut vm, stack, heap) = match vm {
            Ok(info) => info,
            Err(e) => {
                ic_logger_msg!(log_collector, "Failed to create SBF VM: {}", e);
                return Err(Box::new(InstructionError::ProgramEnvironmentSetupFailure));
            }
        };
        create_vm_time.stop();

        vm.context_object_pointer.execute_time = Some(Measure::start("execute"));
        vm.registers[1] = ebpf::MM_INPUT_START;

        // SIMD-0321: Provide offset to instruction data in VM register 2.
        if provide_instruction_data_offset_in_vm_r2 {
            vm.registers[2] = instruction_data_offset as u64;
        }
        let (compute_units_consumed, result) = vm.execute_program(executable, !use_jit);
        let register_trace = std::mem::take(&mut vm.register_trace);
        MEMORY_POOL.with_borrow_mut(|memory_pool| {
            memory_pool.put_stack(stack);
            memory_pool.put_heap(heap);
            debug_assert!(memory_pool.stack_len() <= MAX_INSTRUCTION_STACK_DEPTH);
            debug_assert!(memory_pool.heap_len() <= MAX_INSTRUCTION_STACK_DEPTH);
        });
        drop(vm);
        invoke_context.insert_register_trace(register_trace);
        if let Some(execute_time) = invoke_context.execute_time.as_mut() {
            execute_time.stop();
            invoke_context.timings.execute_us += execute_time.as_us();
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

                if stricter_abi_and_runtime_constraints {
                    if let EbpfError::SyscallError(err) = error {
                        error = err
                            .downcast::<EbpfError>()
                            .map(|err| *err)
                            .unwrap_or_else(EbpfError::SyscallError);
                    }
                    if let EbpfError::AccessViolation(access_type, vm_addr, len, _section_name) =
                        error
                    {
                        // If stricter_abi_and_runtime_constraints is enabled and a program tries to write to a readonly
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
                                error = EbpfError::SyscallError(Box::new(
                                    #[allow(deprecated)]
                                    match access_type {
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
                                    },
                                ));
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
        stricter_abi_and_runtime_constraints: bool,
        account_data_direct_mapping: bool,
    ) -> Result<(), InstructionError> {
        serialization::deserialize_parameters(
            &invoke_context
                .transaction_context
                .get_current_instruction_context()?,
            stricter_abi_and_runtime_constraints,
            account_data_direct_mapping,
            parameter_bytes,
            &invoke_context.get_syscall_context()?.accounts_metadata,
        )
    }

    let mut deserialize_time = Measure::start("deserialize");
    let execute_or_deserialize_result = execution_result.and_then(|_| {
        deserialize_parameters(
            invoke_context,
            parameter_bytes.as_slice(),
            stricter_abi_and_runtime_constraints,
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
