// Cargo clippy runs with 1.81, but cargo-build-sbf is on 1.79.
#![allow(stable_features)]
#![feature(panic_info_message)]

use solana_program::{
    account_info::AccountInfo, entrypoint::ProgramResult, program_error::ProgramError,
    pubkey::Pubkey,
};

solana_program::entrypoint!(process_instruction);

fn process_instruction(
    _program_id: &Pubkey,
    _accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    match instruction_data[0] {
        0 => {
            // Accessing an invalid index creates a dynamic generated panic message.
            let a = instruction_data[92341];
            return Err(ProgramError::Custom(a as u32));
        }
        1 => {
            // Unwrap on a None emit a compiler constant panic message.
            let a: Option<u64> = None;
            #[allow(clippy::unnecessary_literal_unwrap)]
            let b = a.unwrap();
            return Err(ProgramError::Custom(b as u32));
        }

        _ => (),
    }
    Ok(())
}
