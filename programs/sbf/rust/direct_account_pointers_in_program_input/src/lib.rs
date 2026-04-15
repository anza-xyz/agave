//! Test program that reads account pointers directly from the program input.

#![allow(clippy::arithmetic_side_effects)]
#![allow(clippy::missing_safety_doc)]

use {
    core::{
        mem::{MaybeUninit, size_of},
        ptr::with_exposed_provenance_mut,
        slice::from_raw_parts,
    },
    solana_account_info::AccountInfo,
    solana_account_view::AccountView,
    solana_address::Address,
    solana_program_entrypoint::deserialize_into,
    solana_program_error::ProgramError,
};

const BPF_ALIGN_OF_U128: usize = 8;

macro_rules! align_pointer {
    ($ptr:ident) => {
        with_exposed_provenance_mut(
            ($ptr.expose_provenance() + (BPF_ALIGN_OF_U128 - 1)) & !(BPF_ALIGN_OF_U128 - 1),
        )
    };
}

<<<<<<< HEAD
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
=======
const MAX_ACCOUNTS: usize = 64;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn entrypoint(input: *mut u8) -> u64 {
    // First use the current entrypoint to read the actual accounts region
    #[allow(clippy::declare_interior_mutable_const)]
    const UNINIT: MaybeUninit<AccountInfo> = MaybeUninit::<AccountInfo>::uninit();
    let mut uninit_infos = [UNINIT; MAX_ACCOUNTS];

    let (_program_id, num_accounts, instruction_data) =
        unsafe { deserialize_into(input, &mut uninit_infos) };
    let account_infos: &[AccountInfo] =
        unsafe { from_raw_parts(uninit_infos.as_ptr() as *const AccountInfo, num_accounts) };

    // Locate the SIMD-0449 additional pointer slice. It lives immediately
    // after the program id, with up to 7 bytes of padding to reach 8-byte
    // alignment.
    let after_ix_data =
        unsafe { (instruction_data.as_ptr() as *mut u8).add(instruction_data.len()) };
    let after_program_id = unsafe { after_ix_data.add(size_of::<Address>()) };
    let slice_ptr = align_pointer!(after_program_id) as *const AccountView;
    let account_views: &[AccountView] = unsafe { from_raw_parts(slice_ptr, num_accounts) };

    // Check all account views from the additional pointer slice against their
    // account info counterparts.
    for (info, view) in account_infos.iter().zip(account_views.iter()) {
        let mismatch = info.key != view.address()
            || info.owner != view.owner()
            || info.lamports() != view.lamports()
            || info.is_signer != view.is_signer()
            || info.is_writable != view.is_writable()
            || info.executable != view.executable()
            || info.data.borrow().len() != view.data_len();
        if mismatch {
            return ProgramError::Custom(1).into();
>>>>>>> 953d2b03e (fix: SIMD-0449: use proper vm addr in pointer array (#11934))
        }
    }

    solana_cpi::set_return_data(&num_accounts.to_le_bytes());
    solana_program_entrypoint::SUCCESS
}

solana_program_entrypoint::custom_heap_default!();
solana_program_entrypoint::custom_panic_default!();
