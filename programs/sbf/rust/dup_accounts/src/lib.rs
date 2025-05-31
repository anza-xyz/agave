//! Example Rust-based SBF program that tests duplicate accounts passed via accounts

#![allow(clippy::arithmetic_side_effects)]

use {
    solana_account_info::AccountInfo,
    solana_instruction::{AccountMeta, Instruction},
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
    match instruction_data[0] {
        1 => {
            msg!("modify account (2) data");
            accounts[2].data.borrow_mut()[0] = 1;
        }
        2 => {
            msg!("modify account (3) data");
            accounts[3].data.borrow_mut()[0] = 2;
        }
        3 => {
            msg!("modify account (2,3) data");
            accounts[2].data.borrow_mut()[0] += 1;
            accounts[3].data.borrow_mut()[0] += 2;
        }
        4 => {
            msg!("modify account (1,2) lamports");
            **accounts[1].lamports.borrow_mut() -= 1;
            **accounts[2].lamports.borrow_mut() += 1;
        }
        5 => {
            msg!("modify account (1,3) lamports");
            **accounts[1].lamports.borrow_mut() -= 2;
            **accounts[3].lamports.borrow_mut() += 2;
        }
        6 => {
            msg!("modify account (1,2,3) lamports");
            **accounts[1].lamports.borrow_mut() -= 3;
            **accounts[2].lamports.borrow_mut() += 1;
            **accounts[3].lamports.borrow_mut() += 2;
        }
        7 => {
            msg!("check account (0,1,2,3) privs");
            assert!(accounts[0].is_signer);
            assert!(!accounts[1].is_signer);
            assert!(accounts[2].is_signer);
            assert!(accounts[3].is_signer);

            assert!(accounts[0].is_writable);
            assert!(accounts[1].is_writable);
            assert!(accounts[2].is_writable);
            assert!(accounts[3].is_writable);

            if accounts.len() > 4 {
                let instruction = Instruction::new_with_bytes(
                    *program_id,
                    &[7],
                    vec![
                        AccountMeta::new(*accounts[0].key, true),
                        AccountMeta::new(*accounts[1].key, false),
                        AccountMeta::new(*accounts[2].key, false),
                        AccountMeta::new_readonly(*accounts[3].key, true),
                    ],
                );
                invoke(&instruction, accounts)?;

                let instruction = Instruction::new_with_bytes(
                    *program_id,
                    &[3],
                    vec![
                        AccountMeta::new(*accounts[0].key, true),
                        AccountMeta::new(*accounts[1].key, false),
                        AccountMeta::new(*accounts[2].key, false),
                        AccountMeta::new(*accounts[3].key, false),
                    ],
                );
                invoke(&instruction, accounts)?;
                assert_eq!(accounts[2].try_borrow_mut_data()?[0], 3);
                assert_eq!(accounts[3].try_borrow_mut_data()?[0], 3);
            }
        }
        _ => {
            msg!("Unrecognized command");
            return Err(ProgramError::InvalidArgument);
        }
    }
    Ok(())
}
