use {
    super::*,
    solana_instruction::Instruction,
    solana_program_runtime::cpi::{
        SyscallInvokeSigned, TranslatedAccount, cpi_common, translate_accounts_c,
        translate_accounts_rust, translate_instruction_c, translate_instruction_rust,
        translate_signers_c, translate_signers_rust,
    },
};

declare_builtin_function!(
    /// Cross-program invocation called from Rust
    SyscallInvokeSignedRust,
    fn rust(
        invoke_context: &mut InvokeContext,
        instruction_addr: u64,
        account_infos_addr: u64,
        account_infos_len: u64,
        signers_seeds_addr: u64,
        signers_seeds_len: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        cpi_common::<Self>(
            invoke_context,
            instruction_addr,
            account_infos_addr,
            account_infos_len,
            signers_seeds_addr,
            signers_seeds_len,
            memory_mapping,
        )
    }
);

impl SyscallInvokeSigned for SyscallInvokeSignedRust {
    fn translate_instruction(
        addr: u64,
        memory_mapping: &MemoryMapping,
        invoke_context: &mut InvokeContext,
        check_aligned: bool,
    ) -> Result<Instruction, Error> {
        translate_instruction_rust(addr, memory_mapping, invoke_context, check_aligned)
    }

    fn translate_accounts<'a>(
        account_infos_addr: u64,
        account_infos_len: u64,
        memory_mapping: &MemoryMapping,
        invoke_context: &mut InvokeContext,
        check_aligned: bool,
    ) -> Result<Vec<TranslatedAccount<'a>>, Error> {
        translate_accounts_rust(
            account_infos_addr,
            account_infos_len,
            memory_mapping,
            invoke_context,
            check_aligned,
        )
    }

    fn translate_signers(
        program_id: &Pubkey,
        signers_seeds_addr: u64,
        signers_seeds_len: u64,
        memory_mapping: &MemoryMapping,
        check_aligned: bool,
    ) -> Result<Vec<Pubkey>, Error> {
        translate_signers_rust(
            program_id,
            signers_seeds_addr,
            signers_seeds_len,
            memory_mapping,
            check_aligned,
        )
    }
}

declare_builtin_function!(
    /// Cross-program invocation called from C
    SyscallInvokeSignedC,
    fn rust(
        invoke_context: &mut InvokeContext,
        instruction_addr: u64,
        account_infos_addr: u64,
        account_infos_len: u64,
        signers_seeds_addr: u64,
        signers_seeds_len: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        cpi_common::<Self>(
            invoke_context,
            instruction_addr,
            account_infos_addr,
            account_infos_len,
            signers_seeds_addr,
            signers_seeds_len,
            memory_mapping,
        )
    }
);

impl SyscallInvokeSigned for SyscallInvokeSignedC {
    fn translate_instruction(
        addr: u64,
        memory_mapping: &MemoryMapping,
        invoke_context: &mut InvokeContext,
        check_aligned: bool,
    ) -> Result<Instruction, Error> {
        translate_instruction_c(addr, memory_mapping, invoke_context, check_aligned)
    }

    fn translate_accounts<'a>(
        account_infos_addr: u64,
        account_infos_len: u64,
        memory_mapping: &MemoryMapping,
        invoke_context: &mut InvokeContext,
        check_aligned: bool,
    ) -> Result<Vec<TranslatedAccount<'a>>, Error> {
        translate_accounts_c(
            account_infos_addr,
            account_infos_len,
            memory_mapping,
            invoke_context,
            check_aligned,
        )
    }

    fn translate_signers(
        program_id: &Pubkey,
        signers_seeds_addr: u64,
        signers_seeds_len: u64,
        memory_mapping: &MemoryMapping,
        check_aligned: bool,
    ) -> Result<Vec<Pubkey>, Error> {
        translate_signers_c(
            program_id,
            signers_seeds_addr,
            signers_seeds_len,
            memory_mapping,
            check_aligned,
        )
    }
}
