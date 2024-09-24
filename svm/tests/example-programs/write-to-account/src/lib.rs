use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint,
    entrypoint::ProgramResult,
    incinerator,
    program_error::ProgramError,
    pubkey::Pubkey,
};

entrypoint!(process_instruction);

fn process_instruction(
    _program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let accounts_iter = &mut accounts.iter();
    let target_account_info = next_account_info(accounts_iter)?;
    let incinerator_info = next_account_info(accounts_iter)?;
    if !incinerator::check_id(incinerator_info.key) {
        return Err(ProgramError::InvalidAccountData);
    }

    match data[0] {
        // set account data
        0 => {
            let mut account_data = target_account_info.try_borrow_mut_data()?;
            account_data[0] = 100;
        }
        // deallocate account
        1 => {
            let mut target_lamports = target_account_info.try_borrow_mut_lamports()?;
            let mut incinerator_lamports = incinerator_info.try_borrow_mut_lamports()?;

            **incinerator_lamports = incinerator_lamports
                .checked_add(**target_lamports)
                .ok_or(ProgramError::ArithmeticOverflow)?;

            **target_lamports = target_lamports
                .checked_sub(**target_lamports)
                .ok_or(ProgramError::InsufficientFunds)?;
        }
        // bad ixn
        _ => {
            return Err(ProgramError::InvalidArgument);
        }
    }

    Ok(())
}
