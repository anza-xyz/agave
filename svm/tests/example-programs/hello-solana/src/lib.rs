use {
    solana_account_info::AccountInfo, solana_msg::msg, solana_program_entrypoint::entrypoint,
    solana_program_error::ProgramResult, solana_pubkey::Pubkey,
};

entrypoint!(process_instruction);

fn process_instruction(
    _program_id: &Pubkey,
    _accounts: &[AccountInfo],
    _instruction_data: &[u8],
) -> ProgramResult {
    msg!("Hello, Solana!");

    Ok(())
}
