//! Example Rust-based SBF program that exercises error handling

use {
    num_derive::FromPrimitive,
    solana_account_info::AccountInfo,
    solana_msg::msg,
    solana_program_error::{ProgramError, ProgramResult, ToStr},
    solana_pubkey::{Pubkey, PubkeyError},
    thiserror::Error,
};

/// Custom program errors
#[derive(Error, Debug, Clone, PartialEq, FromPrimitive)]
pub enum MyError {
    #[error("Default enum start")]
    DefaultEnumStart,
    #[error("The Answer")]
    TheAnswer = 42,
}
impl From<MyError> for ProgramError {
    fn from(e: MyError) -> Self {
        ProgramError::Custom(e as u32)
    }
}
impl ToStr for MyError {
    fn to_str<E>(&self) -> &'static str
    where
        E: 'static + ToStr + TryFrom<u32>,
    {
        match self {
            MyError::DefaultEnumStart => "Error: Default enum start",
            MyError::TheAnswer => "Error: The Answer",
        }
    }
}

solana_program_entrypoint::entrypoint_no_alloc!(process_instruction);
fn process_instruction(
    _program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    match instruction_data[0] {
        1 => {
            msg!("return success");
            Ok(())
        }
        2 => {
            msg!("return a builtin");
            Err(ProgramError::InvalidAccountData)
        }
        3 => {
            msg!("return default enum start value");
            Err(MyError::DefaultEnumStart.into())
        }
        4 => {
            msg!("return custom error");
            Err(MyError::TheAnswer.into())
        }
        7 => {
            let data = accounts[0].try_borrow_mut_data()?;
            let data2 = accounts[0].try_borrow_mut_data()?;
            assert_eq!(*data, *data2);
            Ok(())
        }
        9 => {
            msg!("return pubkey error");
            Err(PubkeyError::MaxSeedLengthExceeded.into())
        }
        _ => {
            msg!("Unsupported");
            Err(ProgramError::InvalidInstructionData)
        }
    }
}
