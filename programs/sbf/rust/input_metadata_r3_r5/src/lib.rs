//! Test program that reads account pointers directly from the program input.

#![allow(clippy::arithmetic_side_effects)]
#![allow(clippy::missing_safety_doc)]
#![allow(clippy::declare_interior_mutable_const)]

use {
    core::{mem::MaybeUninit, slice::from_raw_parts},
    solana_account_info::AccountInfo,
    solana_account_view::AccountView,
    solana_program_entrypoint::deserialize_into,
    solana_program_error::ProgramError,
};

const MAX_ACCOUNTS: usize = 64;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn entrypoint(
    input: *mut u8,
    instruction_data_addr: *const u8,
    instruction_data_len: u64,
    accounts_addr: *mut u8,
    accounts_len: u64,
) -> u64 {
    // Validate that the lengths of the accounts and instruction data in the program input match
    // the lengths passed in via registers r3 and r5.

    let input_accounts_len = unsafe { *(input as *const u64) };
    let input_instruction_data_len = unsafe { *((instruction_data_addr as u64 - 8) as *const u64) };

    if input_accounts_len != accounts_len || input_instruction_data_len != instruction_data_len {
        return ProgramError::InvalidArgument.into();
    }

    // Validate that the account pointers in the program input match the account pointers passed
    // in via register r4.

    let account_views: &[AccountView] =
        unsafe { from_raw_parts(accounts_addr as *const AccountView, accounts_len as usize) };

    const UNINIT: MaybeUninit<AccountInfo> = MaybeUninit::<AccountInfo>::uninit();
    let mut uninit_infos = [UNINIT; MAX_ACCOUNTS];

    let (_program_id, num_accounts, _instruction_data) =
        unsafe { deserialize_into(input, &mut uninit_infos) };
    let account_infos: &[AccountInfo] =
        unsafe { from_raw_parts(uninit_infos.as_ptr() as *const AccountInfo, num_accounts) };

    for (info, view) in account_infos.iter().zip(account_views.iter()) {
        let mismatch = info.key != view.address()
            || *info.owner != view.owner()
            || info.lamports() != view.lamports()
            || info.is_signer != view.is_signer()
            || info.is_writable != view.is_writable()
            || info.executable != view.executable()
            || info.data.borrow().len() != view.data_len();
        if mismatch {
            return ProgramError::Custom(1).into();
        }
    }

    solana_cpi::set_return_data(&accounts_len.to_le_bytes());
    solana_program_entrypoint::SUCCESS
}

solana_program_entrypoint::custom_heap_default!();
solana_program_entrypoint::custom_panic_default!();
