use {
    super::*,
    solana_instruction::Instruction,
    solana_program_runtime::cpi::{
        SyscallInvokeSigned, TranslatedAccount, cpi_common, translate_accounts_c,
        translate_accounts_rust, translate_instruction_c, translate_instruction_rust,
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
