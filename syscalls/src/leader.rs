use {super::*, crate::translate_mut, solana_sbpf::ebpf, solana_svm_callback::LeaderInfo};

pub struct SyscallGetLeader {}
impl BuiltinFunctionDefinition<InvokeContext<'_, '_>> for SyscallGetLeader {
    type Error = Error;
    fn rust(
        invoke_context: &mut InvokeContext<'_, '_>,
        var_addr: u64,
        _arg2: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
    ) -> Result<u64, Error> {
        let leader_info = invoke_context.get_leader_info();
        let Some(leader_info) = leader_info else {
            // maybe wrong error type to use
            return Err(SyscallError::InvalidAttribute.into());
        };
        let amount = invoke_context
            .get_execution_cost()
            .sysvar_base_cost
            .saturating_add(size_of::<LeaderInfo>() as u64);
        invoke_context.compute_meter.consume_checked(amount)?;

        let check_aligned = invoke_context.get_check_aligned();
        if !check_aligned {
            return Err(SyscallError::UnalignedPointer.into());
        }

        if var_addr >= ebpf::MM_INPUT_START {
            return Err(SyscallError::InvalidPointer.into());
        }

        let memory_mapping = invoke_context.memory_contexts.memory_mapping_mut()?;
        translate_mut!(
            memory_mapping,
            check_aligned,
            let var: (&mut std::mem::MaybeUninit<LeaderInfo>) = map(var_addr)?;
        );

        var.write(leader_info);

        Ok(SUCCESS)
    }
}
