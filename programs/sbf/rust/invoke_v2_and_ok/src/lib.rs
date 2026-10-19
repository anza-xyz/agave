#![allow(clippy::arithmetic_side_effects)]

//! Invokes a noop program using `invoke_signed_v2`` syscall. Any additional account
//! is passed to the noop program.

use {
    core::marker::PhantomData, solana_account_info::AccountInfo,
    solana_program_error::ProgramResult, solana_pubkey::Pubkey,
};

solana_define_syscall::define_syscall!(fn sol_invoke_signed_v2(instruction_addr: u64, signers_seeds_addr: u64, signers_seeds_len: u64) -> u64);

pub fn invoke_signed_v2(instruction: &CpiInstruction, signers: &[&[&[u8]]]) -> u64 {
    unsafe {
        sol_invoke_signed_v2(
            instruction as *const CpiInstruction as u64,
            signers.as_ptr() as u64,
            signers.len() as u64,
        )
    }
}

/// Information about an instruction.
#[repr(C)]
#[derive(Debug, Clone)]
pub struct CpiInstruction<'accounts, 'address, 'data> {
    /// Public key of the program.
    program_id: *const Pubkey,

    /// Accounts expected by the program instruction.
    accounts: *const CpiAccount,

    /// Number of accounts expected by the program instruction.
    accounts_len: u64,

    /// Data expected by the program instruction.
    data: *const u8,

    /// Length of the data expected by the program instruction.
    data_len: u64,

    _accounts_phanton: PhantomData<&'accounts [CpiAccount]>,

    _data_phanton: PhantomData<&'data [u8]>,

    _program_id: PhantomData<&'address Pubkey>,
}

impl<'accounts, 'address, 'data> CpiInstruction<'accounts, 'address, 'data> {
    #[inline(always)]
    pub fn new(
        program_id: &'address Pubkey,
        accounts: &'accounts [CpiAccount],
        data: &'data [u8],
    ) -> Self {
        Self {
            program_id,
            accounts: accounts.as_ptr(),
            accounts_len: accounts.len() as u64,
            data: data.as_ptr(),
            data_len: data.len() as u64,
            _accounts_phanton: PhantomData,
            _data_phanton: PhantomData,
            _program_id: PhantomData,
        }
    }
}

/// Mask for the writable flag.
const WRITABLE_MASK: u32 = 1u32 << 16;

/// Mask for the signer flag.
const SIGNER_MASK: u32 = 1u32 << 24;

#[repr(C)]
#[derive(Debug, Clone)]
pub struct CpiAccount(u32);

impl CpiAccount {
    /// Create an account from its transaction index and instruction flags.
    #[inline(always)]
    pub const fn new(index: u16, is_signer: bool, is_writable: bool) -> Self {
        Self(index as u32 | (SIGNER_MASK * is_signer as u32) | (WRITABLE_MASK * is_writable as u32))
    }
}

solana_program_entrypoint::entrypoint_no_alloc!(process_instruction);
fn process_instruction(
    _program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    let to_call = accounts[0].key;
    let cpi_accounts: Vec<CpiAccount> = accounts[1..]
        .iter()
        .enumerate()
        .map(|(index, account)| {
            CpiAccount::new(
                // +1 because the first account is the program
                // being called.
                (index + 1) as u16,
                account.is_signer,
                account.is_writable,
            )
        })
        .collect();
    let cpi_instruction = CpiInstruction::new(to_call, &cpi_accounts, instruction_data);

    let _ = invoke_signed_v2(&cpi_instruction, &[]);

    Ok(())
}
