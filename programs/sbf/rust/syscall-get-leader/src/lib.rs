//! Example Rust-based SBF program that tests the `sol_get_leader` syscall.

use {
    solana_account_info::AccountInfo,
    solana_leader_info::{LeaderInfo, get_leader_info},
    solana_program::program::set_return_data,
    solana_program_error::ProgramResult,
    solana_pubkey::Pubkey,
};

solana_program_entrypoint::entrypoint_no_alloc!(process_instruction);
pub fn process_instruction(
    _program_id: &Pubkey,
    _accounts: &[AccountInfo],
    _instruction_data: &[u8],
) -> ProgramResult {
    let leader_info = get_leader_info();

    // Return the fields in `LeaderInfo` order so the test can check the layout.
    let mut data = [0u8; LeaderInfo::SIZE];
    for (chunk, address) in data.chunks_exact_mut(32).zip([
        &leader_info.leader_identity,
        &leader_info.leader_vote,
        &leader_info.next_leader_identity,
        &leader_info.next_leader_vote,
    ]) {
        chunk.copy_from_slice(address.as_ref());
    }
    set_return_data(&data);
    Ok(())
}
