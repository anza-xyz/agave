use super::*;

fn get_sysvar<T: std::fmt::Debug + Sysvar + SysvarId + Clone>(
    sysvar: Result<Arc<T>, InstructionError>,
    var_addr: u64,
    check_aligned: bool,
    memory_mapping: &mut MemoryMapping,
    invoke_context: &mut InvokeContext,
) -> Result<u64, Error> {
    consume_compute_meter(
        invoke_context,
        invoke_context
            .get_compute_budget()
            .sysvar_base_cost
            .saturating_add(size_of::<T>() as u64),
    )?;
    let var = translate_type_mut::<T>(memory_mapping, var_addr, check_aligned)?;

    let sysvar: Arc<T> = sysvar?;
    *var = T::clone(sysvar.as_ref());

    Ok(SUCCESS)
}

declare_builtin_function!(
    /// Get a Clock sysvar
    SyscallGetClockSysvar,
    fn rust(
        invoke_context: &mut InvokeContext,
        var_addr: u64,
        _arg2: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        get_sysvar(
            invoke_context.get_sysvar_cache().get_clock(),
            var_addr,
            invoke_context.get_check_aligned(),
            memory_mapping,
            invoke_context,
        )
    }
);

declare_builtin_function!(
    /// Get a EpochSchedule sysvar
    SyscallGetEpochScheduleSysvar,
    fn rust(
        invoke_context: &mut InvokeContext,
        var_addr: u64,
        _arg2: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        get_sysvar(
            invoke_context.get_sysvar_cache().get_epoch_schedule(),
            var_addr,
            invoke_context.get_check_aligned(),
            memory_mapping,
            invoke_context,
        )
    }
);

declare_builtin_function!(
    /// Get a EpochRewards sysvar
    SyscallGetEpochRewardsSysvar,
    fn rust(
        invoke_context: &mut InvokeContext,
        var_addr: u64,
        _arg2: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        get_sysvar(
            invoke_context.get_sysvar_cache().get_epoch_rewards(),
            var_addr,
            invoke_context.get_check_aligned(),
            memory_mapping,
            invoke_context,
        )
    }
);

declare_builtin_function!(
    /// Get a Fees sysvar
    SyscallGetFeesSysvar,
    fn rust(
        invoke_context: &mut InvokeContext,
        var_addr: u64,
        _arg2: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        #[allow(deprecated)]
        {
            get_sysvar(
                invoke_context.get_sysvar_cache().get_fees(),
                var_addr,
                invoke_context.get_check_aligned(),
                memory_mapping,
                invoke_context,
            )
        }
    }
);

declare_builtin_function!(
    /// Get a Rent sysvar
    SyscallGetRentSysvar,
    fn rust(
        invoke_context: &mut InvokeContext,
        var_addr: u64,
        _arg2: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        get_sysvar(
            invoke_context.get_sysvar_cache().get_rent(),
            var_addr,
            invoke_context.get_check_aligned(),
            memory_mapping,
            invoke_context,
        )
    }
);

declare_builtin_function!(
    /// Get a Last Restart Slot sysvar
    SyscallGetLastRestartSlotSysvar,
    fn rust(
        invoke_context: &mut InvokeContext,
        var_addr: u64,
        _arg2: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        get_sysvar(
            invoke_context.get_sysvar_cache().get_last_restart_slot(),
            var_addr,
            invoke_context.get_check_aligned(),
            memory_mapping,
            invoke_context,
        )
    }
);

declare_builtin_function!(
    /// Get a slice of a Sysvar in-memory representation
    SyscallGetSysvar,
    fn rust(
        invoke_context: &mut InvokeContext,
        // XXX stakehistory just for now
        sysvar_tag: u64,
        length: u64,
        offset: u64,
        var_addr: u64,
        _arg5: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        consume_compute_meter(
            invoke_context,
            invoke_context
                .get_compute_budget()
                .sysvar_base_cost
                .saturating_add(length as u64),
        )?;

        match sysvar_tag {
            // stake history
            _ => {
                let stake_history = invoke_context.get_sysvar_cache().get_stake_history()?;

                if length == 4 && offset == 0 {
                    let var = translate_type_mut::<u32>(
                        memory_mapping,
                        var_addr,
                        invoke_context.get_check_aligned(),
                    )?;
                    *var = stake_history.len().try_into()?;
                } else {
                    if length % 32 != 0 {
                        panic!("misaligned length");
                    }

                    if offset % 32 != 4 {
                        panic!("misaligned offset");
                    }

                    if length == 0 {
                        panic!("zero length");
                    }

                    if length + offset > stake_history.len() as u64 * 32 + 4 {
                        panic!("oob read");
                    }

                    let var = translate_slice_mut::<u8>(
                        memory_mapping,
                        var_addr,
                        length,
                        invoke_context.get_check_aligned(),
                    )?;

                    let vec_length = length as usize / 32;
                    let vec_i0 = (offset as usize - 4) / 32;

                    println!(
                        "HANA veclen: {}, i0: {}, length: {}, offset: {}, sh len: {}",
                        vec_length,
                        vec_i0,
                        length,
                        offset,
                        stake_history.len()
                    );
                    for i in vec_i0..vec_i0 + vec_length {
                        let (epoch, entry) = &stake_history[i];
                        let output_pos = (i - vec_i0) * 32;

                        println!(
                            "HANA inside syscall epoch: {}, entry: {:?}, pos: {}",
                            epoch, entry, output_pos
                        );

                        var[output_pos..output_pos + 8].copy_from_slice(&epoch.to_le_bytes());
                        var[output_pos + 8..output_pos + 16]
                            .copy_from_slice(&entry.effective.to_le_bytes());
                        var[output_pos + 16..output_pos + 24]
                            .copy_from_slice(&entry.activating.to_le_bytes());
                        var[output_pos + 24..output_pos + 32]
                            .copy_from_slice(&entry.deactivating.to_le_bytes());
                    }
                }
            }
        }

        Ok(SUCCESS)
    }
);
