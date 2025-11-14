//! @brief Example Rust-based BPF program that exercises the sol_remaining_compute_units syscall

use {
    solana_account_info::AccountInfo, solana_msg::msg,
    solana_program::compute_units::sol_remaining_compute_units,
    solana_program_error::ProgramResult, solana_pubkey::Pubkey,
};
solana_program_entrypoint::entrypoint_no_alloc!(process_instruction);
pub fn process_instruction(
    _program_id: &Pubkey,
    _accounts: &[AccountInfo],
    _instruction_data: &[u8],
) -> ProgramResult {
    let mut i = 0u32;
    for _ in 0..100_000 {
        if i.is_multiple_of(500) {
            let remaining = sol_remaining_compute_units();
            msg!("remaining compute units: {:?}", remaining);
            if remaining < 25_000 {
                break;
            }
        }
        i = i.saturating_add(1);
    }

    msg!("i: {:?}", i);

    Ok(())
}
