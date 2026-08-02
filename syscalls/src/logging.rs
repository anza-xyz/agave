use {
    super::*, solana_program_runtime::memory::translate_vm_slice, solana_sbpf::vm::ContextObject,
};

declare_builtin_function!(
    /// Log a user's info message
    SyscallLog,
    fn rust(
        invoke_context: &mut InvokeContext<'_, '_>,
        addr: u64,
        len: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
    ) -> Result<u64, Error> {
        let cost = invoke_context
            .get_execution_cost()
            .syscall_base_cost
            .max(len);
        invoke_context.compute_meter.consume_checked(cost)?;

        let check_aligned = invoke_context.get_check_aligned();
        let memory_mapping = invoke_context.memory_contexts.memory_mapping()?;
        translate_string_and_do(
            memory_mapping,
            addr,
            len,
            check_aligned,
            &mut |string: &str| {
                stable_log::program_log(&invoke_context.get_log_collector(), string);
                Ok(0)
            },
        )?;
        Ok(0)
    }
);

declare_builtin_function!(
    /// Log 5 64-bit values
    SyscallLogU64,
    fn rust(
        invoke_context: &mut InvokeContext<'_, '_>,
        arg1: u64,
        arg2: u64,
        arg3: u64,
        arg4: u64,
        arg5: u64,
    ) -> Result<u64, Error> {
        let cost = invoke_context.get_execution_cost().log_64_units;
        invoke_context.compute_meter.consume_checked(cost)?;

        stable_log::program_log(
            &invoke_context.get_log_collector(),
            &format!("{arg1:#x}, {arg2:#x}, {arg3:#x}, {arg4:#x}, {arg5:#x}"),
        );
        Ok(0)
    }
);

declare_builtin_function!(
    /// Log current compute consumption
    SyscallLogBpfComputeUnits,
    fn rust(
        invoke_context: &mut InvokeContext<'_, '_>,
        _arg1: u64,
        _arg2: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
    ) -> Result<u64, Error> {
        let cost = invoke_context.get_execution_cost().syscall_base_cost;
        invoke_context.compute_meter.consume_checked(cost)?;

        ic_logger_msg!(
            invoke_context.get_log_collector(),
            "Program consumption: {} units remaining",
            invoke_context.get_remaining(),
        );
        Ok(0)
    }
);

declare_builtin_function!(
    /// Log a [`Pubkey`] as a base58 string
    SyscallLogPubkey,
    fn rust(
        invoke_context: &mut InvokeContext<'_, '_>,
        pubkey_addr: u64,
        _arg2: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
    ) -> Result<u64, Error> {
        let cost = invoke_context.get_execution_cost().log_pubkey_units;
        invoke_context.compute_meter.consume_checked(cost)?;

        let check_aligned = invoke_context.get_check_aligned();
        let memory_mapping = invoke_context.memory_contexts.memory_mapping()?;
        let pubkey = translate_type::<Pubkey>(
            memory_mapping,
            pubkey_addr,
            check_aligned,
        )?;
        stable_log::program_log(&invoke_context.get_log_collector(), &pubkey.to_string());
        Ok(0)
    }
);

declare_builtin_function!(
    /// Log data handling
    SyscallLogData,
    fn rust(
        invoke_context: &mut InvokeContext<'_, '_>,
        addr: u64,
        len: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
    ) -> Result<u64, Error> {
        let execution_cost = invoke_context.get_execution_cost();

        invoke_context.compute_meter.consume_checked(execution_cost.syscall_base_cost)?;

        let check_aligned = invoke_context.get_check_aligned();
        let memory_mapping = invoke_context.memory_contexts.memory_mapping()?;
        let untranslated_fields = translate_slice::<VmSlice<u8>>(
            memory_mapping,
            addr,
            len,
            check_aligned,
        )?;

        let cost = execution_cost
            .syscall_base_cost
            .saturating_mul(untranslated_fields.len() as u64);
        invoke_context.compute_meter.consume_checked(cost)?;
        let cost = untranslated_fields
            .iter()
            .fold(0u64, |total, e| total.saturating_add(e.len()));
        invoke_context.compute_meter.consume_checked(cost)?;

        let mut fields = Vec::with_capacity(untranslated_fields.len());

        for untranslated_field in untranslated_fields {
            fields.push(translate_vm_slice(untranslated_field, memory_mapping, check_aligned)?);
        }

        let log_collector = invoke_context.get_log_collector();

        stable_log::program_data(&log_collector, &fields);

        Ok(0)
    }
);
