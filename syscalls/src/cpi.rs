use {
    super::*,
    solana_instruction::Instruction,
    solana_program_runtime::cpi::{
        SolInstruction, SyscallInvokeSigned, TranslatedAccount, cpi_common, translate_accounts_c,
        translate_accounts_rust, translate_accounts_v2, translate_instruction_c,
        translate_instruction_rust, translate_instruction_v2,
    },
};

/// Cross-program invocation called from Rust
pub struct SyscallInvokeSignedRust {}
impl BuiltinFunctionDefinition<InvokeContext<'_, '_>> for SyscallInvokeSignedRust {
    type Error = Error;
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
}

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

/// Cross-program invocation called from C
pub struct SyscallInvokeSignedC {}
impl BuiltinFunctionDefinition<InvokeContext<'_, '_>> for SyscallInvokeSignedC {
    type Error = Error;
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
}

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

/// Cross-program invocation called from `sol_invoke_signed_v2`
pub struct SyscallInvokeSignedV2 {}
impl BuiltinFunctionDefinition<InvokeContext<'_, '_>> for SyscallInvokeSignedV2 {
    type Error = Error;
    fn rust(
        invoke_context: &mut InvokeContext<'_, '_>,
        instruction_addr: u64,
        signers_seeds_addr: u64,
        signers_seeds_len: u64,
        _arg4: u64,
        _arg5: u64,
    ) -> Result<u64, Error> {
        let check_aligned = invoke_context.get_check_aligned();
        let memory_mapping = invoke_context.memory_contexts.memory_mapping()?;
        let ix_c =
            translate_type::<SolInstruction>(memory_mapping, instruction_addr, check_aligned)?;

        cpi_common::<Self>(
            invoke_context,
            instruction_addr,
            ix_c.accounts_addr,
            ix_c.accounts_len,
            signers_seeds_addr,
            signers_seeds_len,
        )
    }
}

impl SyscallInvokeSigned for SyscallInvokeSignedV2 {
    fn translate_instruction(
        addr: u64,
        invoke_context: &InvokeContext,
    ) -> Result<Instruction, Error> {
        translate_instruction_v2(addr, invoke_context)
    }

    fn translate_accounts<'a>(
        account_metas_addr: u64,
        account_metas_len: u64,
        invoke_context: &InvokeContext,
    ) -> Result<Vec<TranslatedAccount<'a>>, Error> {
        translate_accounts_v2(account_metas_addr, account_metas_len, invoke_context)
    }
}
