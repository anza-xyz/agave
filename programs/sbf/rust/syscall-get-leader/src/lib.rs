//! Example Rust-based SBF program that tests the `sol_get_leader` syscall.

use {
    solana_account_info::AccountInfo, solana_define_syscall::define_syscall,
    solana_program::program::set_return_data, solana_program_error::ProgramResult,
    solana_pubkey::Pubkey,
};

define_syscall!(fn sol_get_leader(var_addr: *mut u8) -> u64);

solana_program_entrypoint::entrypoint_no_alloc!(process_instruction);
pub fn process_instruction(
    _program_id: &Pubkey,
    _accounts: &[AccountInfo],
    _instruction_data: &[u8],
) -> ProgramResult {
    // `LeaderInfo` is four pubkeys, 128 bytes, little-endian field order.
    let mut leader_info = [0u8; 128];
    let result = unsafe { sol_get_leader(leader_info.as_mut_ptr()) };
    assert_eq!(result, 0);
    set_return_data(&leader_info);
    Ok(())
}
