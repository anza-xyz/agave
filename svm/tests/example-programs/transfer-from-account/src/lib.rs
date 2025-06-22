use {
    solana_account_info::{next_account_info, AccountInfo},
    solana_program::program::invoke,
    solana_program_entrypoint::entrypoint,
    solana_program_error::ProgramResult,
    solana_pubkey::Pubkey,
    solana_system_interface::instruction as system_instruction,
};

entrypoint!(process_instruction);

fn process_instruction(
    _program_id: &Pubkey,
    accounts: &[AccountInfo],
    _data: &[u8],
) -> ProgramResult {
    let accounts_iter = &mut accounts.iter();
    let payer = next_account_info(accounts_iter)?;
    let recipient = next_account_info(accounts_iter)?;
    let data_account = next_account_info(accounts_iter)?;
    let system_program = next_account_info(accounts_iter)?;

    let amount = u64::from_le_bytes(data_account.data.borrow()[0..8].try_into().unwrap());

    invoke(
        &system_instruction::transfer(payer.key, recipient.key, amount),
        &[payer.clone(), recipient.clone(), system_program.clone()],
    )?;

    Ok(())
}
