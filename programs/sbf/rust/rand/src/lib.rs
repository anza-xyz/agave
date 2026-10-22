//! Example Rust-based SBF program that tests rand behavior

#![allow(unreachable_code)]

use {
    solana_account_info::AccountInfo, solana_msg::msg, solana_program_error::ProgramResult,
    solana_pubkey::Pubkey,
};

solana_program_entrypoint::entrypoint_no_alloc!(process_instruction);
fn process_instruction(
    _program_id: &Pubkey,
    _accounts: &[AccountInfo],
    _instruction_data: &[u8],
) -> ProgramResult {
    msg!("rand");
    Ok(())
}
