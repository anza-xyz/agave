//! Test program that reads account pointers directly from the program input.

#![allow(clippy::arithmetic_side_effects)]
#![allow(clippy::missing_safety_doc)]

use {
    core::{mem::size_of, ptr::with_exposed_provenance_mut, slice::from_raw_parts},
    solana_account_view::AccountView,
    solana_address::Address,
    solana_program_error::{ProgramError, ProgramResult},
};

/// `assert_eq(core::mem::align_of::<u128>(), 8)` is true for BPF but not
/// for some host machines.
const BPF_ALIGN_OF_U128: usize = 8;

/// Align a pointer to the BPF alignment of [`u128`].
macro_rules! align_pointer {
    ($ptr:ident) => {
        with_exposed_provenance_mut(
            ($ptr.expose_provenance() + (BPF_ALIGN_OF_U128 - 1)) & !(BPF_ALIGN_OF_U128 - 1),
        )
    };
}

#[no_mangle]
pub unsafe extern "C" fn entrypoint(program_input: *mut u8, instruction_data: *mut u8) -> u64 {
    // First 8-bytes of program_input contains the number of accounts.
    let accounts = program_input as *mut u64;

    // The 8-bytes before the instruction data contains the length of
    // the instruction data, even if the instruction data is empty.
    let ix_data_len = *(instruction_data.sub(size_of::<u64>()) as *mut u64) as usize;

    // The program_id is located right after the instruction data.
    let program_id = &*(instruction_data.add(ix_data_len) as *const Address);

    // The slice of account pointers is located right after the program_id.
    let slice_ptr = instruction_data.add(ix_data_len + size_of::<Address>());
    let accounts = from_raw_parts(
        align_pointer!(slice_ptr) as *const AccountView,
        *accounts as usize,
    );

    // The instruction data slice.
    let instruction_data = from_raw_parts(instruction_data, ix_data_len);

    match process_instruction(program_id, accounts, instruction_data) {
        Ok(_) => solana_program_entrypoint::SUCCESS,
        Err(e) => e.into(),
    }
}

pub fn process_instruction(
    _program_id: &Address,
    accounts: &[AccountView],
    _instruction_data: &[u8],
) -> ProgramResult {
    // The program expects the accounts to be duplicated in the input.
    let non_duplicated = accounts.len() / 2;

    for i in 0..non_duplicated {
        let account = &accounts[i];
        let duplicate = &accounts[i + non_duplicated];

        if account != duplicate || account.address() != duplicate.address() {
            return Err(ProgramError::InvalidAccountData);
        }
    }

    solana_cpi::set_return_data(&accounts.len().to_le_bytes());
    Ok(())
}

solana_program_entrypoint::custom_heap_default!();
solana_program_entrypoint::custom_panic_default!();
