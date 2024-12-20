//! Example/test program to trigger vm error by dividing by zero

#![feature(asm_experimental_arch)]

extern crate solana_program;
use {
    solana_program::{account_info::AccountInfo, entrypoint::ProgramResult, pubkey::Pubkey},
    std::arch::asm,
};

solana_program::entrypoint_no_alloc!(process_instruction);
fn process_instruction(
    _program_id: &Pubkey,
    _accounts: &[AccountInfo],
    _instruction_data: &[u8],
) -> ProgramResult {
    unsafe {
        asm!(
            "
            mov64 r0, 0
            mov64 r1, 0
            div64 r0, r1
        "
        );
    }
    Ok(())
}
