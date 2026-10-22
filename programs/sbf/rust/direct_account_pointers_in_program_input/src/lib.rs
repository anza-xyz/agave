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
        }
    }

    solana_cpi::set_return_data(&num_accounts.to_le_bytes());
    solana_program_entrypoint::SUCCESS
}

solana_program_entrypoint::custom_heap_default!();
solana_program_entrypoint::custom_panic_default!();
