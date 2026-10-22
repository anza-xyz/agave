use {
    solana_account_info::AccountInfo,
    solana_program::program::set_return_data,
    solana_program_entrypoint::entrypoint,
    solana_program_error::ProgramResult,
    solana_pubkey::Pubkey,
    solana_sysvar::{clock::Clock, Sysvar},
};

entrypoint!(process_instruction);

fn process_instruction(
    _program_id: &Pubkey,
    _accounts: &[AccountInfo],
    _instruction_data: &[u8],
) -> ProgramResult {
    let time_now = Clock::get().unwrap().unix_timestamp;
    let return_data = time_now.to_be_bytes();
    set_return_data(&return_data);
    Ok(())
}
