//! Example Rust-based SBF program that exercises instruction introspection

use {
    solana_account_info::AccountInfo,
    solana_instruction::{AccountMeta, Instruction},
    solana_instructions_sysvar as instructions,
    solana_msg::msg,
    solana_program::program::invoke,
    solana_program_error::{ProgramError, ProgramResult},
    solana_pubkey::Pubkey,
};

solana_program_entrypoint::entrypoint_no_alloc!(process_instruction);
fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    if instruction_data.is_empty() {
        return Err(ProgramError::InvalidAccountData);
    }

    let secp_instruction_index = instruction_data[0];
    let instructions_account = accounts.last().ok_or(ProgramError::NotEnoughAccountKeys)?;
    assert_eq!(*instructions_account.key, instructions::id());
    let data_len = instructions_account.try_borrow_data()?.len();
    if data_len < 2 {
        return Err(ProgramError::InvalidAccountData);
    }

    let instruction = instructions::load_instruction_at_checked(
        secp_instruction_index as usize,
        instructions_account,
    )?;

    let current_instruction = instructions::load_current_index_checked(instructions_account)?;
    let my_index = instruction_data[1] as u16;
    assert_eq!(current_instruction, my_index);

    msg!(&format!("id: {}", instruction.program_id));

    msg!(&format!("data[0]: {}", instruction.data[0]));
    msg!(&format!("index: {current_instruction}"));

    if instruction_data.len() == 2 {
        // CPI ourself with the same arguments to confirm the instructions sysvar reports the same
        // results from within a CPI
        invoke(
            &Instruction::new_with_bytes(
                *program_id,
                &[instruction_data[0], instruction_data[1], 1],
                vec![AccountMeta::new_readonly(instructions::id(), false)],
            ),
            std::slice::from_ref(instructions_account),
        )?;
    }

    Ok(())
}
