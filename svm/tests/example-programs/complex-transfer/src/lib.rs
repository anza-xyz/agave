use {
    solana_account_info::{next_account_info, AccountInfo},
    solana_program::program::invoke,
    solana_program_entrypoint::entrypoint,
    solana_program_error::ProgramResult,
    solana_pubkey::Pubkey,
    solana_system_interface::instruction as system_instruction,
};

entrypoint!(process_instruction);

// Transfers `instruction_amount + extra_account_amount` lamports from the payer
// to the recipient, where the recipient and the extra account are supplied via
// an address lookup table. The instruction amount is big-endian and the extra
// account's amount is little-endian.
fn process_instruction(
    _program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let instruction_amount = u64::from_be_bytes(data[0..8].try_into().unwrap());

    let accounts_iter = &mut accounts.iter();
    let payer = next_account_info(accounts_iter)?;
    let recipient = next_account_info(accounts_iter)?;
    let system_program = next_account_info(accounts_iter)?;
    let extra_account = next_account_info(accounts_iter)?;

    let extra_amount = u64::from_le_bytes(extra_account.data.borrow()[0..8].try_into().unwrap());

    invoke(
        &system_instruction::transfer(
            payer.key,
            recipient.key,
            instruction_amount + extra_amount,
        ),
        &[payer.clone(), recipient.clone(), system_program.clone()],
    )?;

    Ok(())
}
