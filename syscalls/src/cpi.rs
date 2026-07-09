use {
    super::*,
    solana_instruction::Instruction,
    solana_program_runtime::cpi::{
        SyscallInvokeSigned, TranslatedAccount, cpi_common, translate_accounts_c,
        translate_accounts_rust, translate_instruction_c, translate_instruction_rust,
        translate_signers,
    },
};

declare_builtin_function!(
    /// Cross-program invocation called from Rust
    SyscallInvokeSignedRust,
    fn rust(
        invoke_context: &mut InvokeContext<'_, '_>,
        instruction_addr: u64,
        account_infos_addr: u64,
        account_infos_len: u64,
        signers_seeds_addr: u64,
        signers_seeds_len: u64,
    ) -> Result<u64, Error> {
        cpi_common::<Self>(
            invoke_context,
            instruction_addr,
            account_infos_addr,
            account_infos_len,
            signers_seeds_addr,
            signers_seeds_len,
        )
    }
);

impl SyscallInvokeSigned for SyscallInvokeSignedRust {
    fn translate_instruction(
        addr: u64,
        invoke_context: &InvokeContext,
    ) -> Result<Instruction, Error> {
        translate_instruction_rust(addr, invoke_context)
    }

    fn translate_accounts<'a>(
        account_infos_addr: u64,
        account_infos_len: u64,
        invoke_context: &InvokeContext,
    ) -> Result<Vec<TranslatedAccount<'a>>, Error> {
        translate_accounts_rust(account_infos_addr, account_infos_len, invoke_context)
    }
}

declare_builtin_function!(
    /// Cross-program invocation called from C
    SyscallInvokeSignedC,
    fn rust(
        invoke_context: &mut InvokeContext<'_, '_>,
        instruction_addr: u64,
        account_infos_addr: u64,
        account_infos_len: u64,
        signers_seeds_addr: u64,
        signers_seeds_len: u64,
    ) -> Result<u64, Error> {
        cpi_common::<Self>(
            invoke_context,
            instruction_addr,
            account_infos_addr,
            account_infos_len,
            signers_seeds_addr,
            signers_seeds_len,
        )
    }
);

impl SyscallInvokeSigned for SyscallInvokeSignedC {
    fn translate_instruction(
        addr: u64,
        invoke_context: &InvokeContext,
    ) -> Result<Instruction, Error> {
        translate_instruction_c(addr, invoke_context)
    }

    fn translate_accounts<'a>(
        account_infos_addr: u64,
        account_infos_len: u64,
        invoke_context: &InvokeContext,
    ) -> Result<Vec<TranslatedAccount<'a>>, Error> {
        translate_accounts_c(account_infos_addr, account_infos_len, invoke_context)
    }
}

declare_builtin_function!(
    /// Cross-program invocation called from ABIv2
    SyscallInvokeSignedV2,
    fn rust(
        invoke_context: &mut InvokeContext<'_, '_>,
        program_idx_in_tx: u64,
        signers_seeds_addr: u64,
        signers_seeds_len: u64,
        _arg4: u64,
        _arg5: u64,
    ) -> Result<u64, Error> {
        // Deduct cost
        let compute_cost = invoke_context.get_execution_cost();
        invoke_context.compute_meter.consume_checked(compute_cost.abi_v2_cpi_base)?;

        // Configure instruction frame
        let callee_program_index_in_tx = u16::try_from(program_idx_in_tx).map_err(|_| InstructionError::MissingAccount)?;
        invoke_context.transaction_context.build_abi_v2_frame(callee_program_index_in_tx)?;

        // This check also verifies that the program account is in the transaction
        let caller_program_id = invoke_context.transaction_context.get_current_instruction_context()?.get_program_key()?;

        // Convert seeds
        let signers = translate_signers(caller_program_id, signers_seeds_addr, signers_seeds_len, invoke_context)?;

        // Invoke program
        invoke_context.internal_native_invoke(&signers)?;
        Ok(0)
    }
);
