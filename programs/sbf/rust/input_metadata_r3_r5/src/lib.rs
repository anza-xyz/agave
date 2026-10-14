//! Test program that reads account pointers directly from the program input.

#![allow(clippy::arithmetic_side_effects)]
#![allow(clippy::missing_safety_doc)]

use solana_program_error::ProgramError;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn entrypoint(
    input: *mut u8,
    instruction_data_addr: *const u8,
    instruction_data_len: u64,
    accounts_addr: *mut u8,
    accounts_len: u64,
) -> u64 {
    let input_accounts_len = unsafe { *(input as *const u64) };
    let input_instruction_data_len = unsafe { *((instruction_data_addr as u64 - 8) as *const u64) };

    if input_accounts_len != accounts_len || input_instruction_data_len != instruction_data_len {
        return ProgramError::InvalidArgument.into();
    }

    let first_account_addr = input as u64 + 8;

    if accounts_len > 0 && first_account_addr != unsafe { *(accounts_addr as *const u64) } {
        return ProgramError::InvalidArgument.into();
    }

    solana_cpi::set_return_data(&accounts_len.to_le_bytes());
    solana_program_entrypoint::SUCCESS
}

solana_program_entrypoint::custom_heap_default!();
solana_program_entrypoint::custom_panic_default!();
