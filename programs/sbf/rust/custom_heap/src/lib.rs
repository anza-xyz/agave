//! Example Rust-based SBF that tests out using a custom heap

#![allow(clippy::arithmetic_side_effects)]

use {
    solana_account_info::AccountInfo,
    solana_msg::msg,
    solana_program_error::ProgramResult,
    solana_pubkey::Pubkey,
    std::{
        alloc::{Layout, alloc},
        mem::align_of,
    },
};
#[cfg(target_os = "solana")]
use {
    solana_program_entrypoint::{HEAP_LENGTH, HEAP_START_ADDRESS},
    std::{mem::size_of, ptr::null_mut},
};

/// Developers can implement their own heap by defining their own
/// `#[global_allocator]`.  The following implements a dummy for test purposes
/// but can be flushed out with whatever the developer sees fit.
#[cfg(target_os = "solana")]
struct BumpAllocator;
#[cfg(target_os = "solana")]
unsafe impl std::alloc::GlobalAlloc for BumpAllocator {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() == isize::MAX as usize - 0x42 {
            // Return test value
            0x42 as *mut u8
        } else {
            const POS_PTR: *mut usize = HEAP_START_ADDRESS as *mut usize;
            const TOP_ADDRESS: usize = HEAP_START_ADDRESS as usize + HEAP_LENGTH;
            const BOTTOM_ADDRESS: usize = HEAP_START_ADDRESS as usize + size_of::<*mut u8>();

            let mut pos = *POS_PTR;
            if pos == 0 {
                // First time, set starting position
                pos = TOP_ADDRESS;
            }
            pos = pos.saturating_sub(layout.size());
            pos &= !(layout.align().saturating_sub(1));
            if pos < BOTTOM_ADDRESS {
                return null_mut();
            }
            *POS_PTR = pos;
            pos as *mut u8
        }
    }
    #[inline]
    unsafe fn dealloc(&self, _: *mut u8, _: Layout) {
        // I'm a bump allocator, I don't free
    }
}
#[cfg(target_os = "solana")]
#[global_allocator]
static A: BumpAllocator = BumpAllocator;

solana_program_entrypoint::entrypoint_no_alloc!(process_instruction);
pub fn process_instruction(
    _program_id: &Pubkey,
    _accounts: &[AccountInfo],
    _instruction_data: &[u8],
) -> ProgramResult {
    msg!("Custom heap");
    unsafe {
        let layout = Layout::from_size_align(isize::MAX as usize - 0x42, align_of::<u8>()).unwrap();
        let ptr = alloc(layout);
        assert_eq!(ptr as u64, 0x42);
    }
    Ok(())
}
