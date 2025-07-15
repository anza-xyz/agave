//! Example Rust-based SBF upgraded program

use {
    solana_account_info::AccountInfo, solana_msg::msg, solana_program_error::ProgramResult,
    solana_pubkey::Pubkey, solana_sysvar::clock,
};

solana_program_entrypoint::entrypoint_no_alloc!(process_instruction);
fn process_instruction(
    _program_id: &Pubkey,
    accounts: &[AccountInfo],
    _instruction_data: &[u8],
) -> ProgramResult {
    msg!("Upgraded program");
    assert_eq!(accounts.len(), 1);
    assert_eq!(*accounts[0].key, clock::id());
    Err(43.into())
}
