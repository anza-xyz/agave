//! Example/test program to trigger vm error by dividing by zero

#![cfg(target_os = "solana")]
#![feature(asm_experimental_arch)]

extern crate solana_program;
use {solana_program::entrypoint::SUCCESS, std::arch::asm};

#[no_mangle]
pub extern "C" fn entrypoint(_input: *mut u8) -> u64 {
    unsafe {
        asm!(
            "
            mov64 r0, 0
            mov64 r1, 0
            div64 r0, r1
        "
        );
    }

    SUCCESS
}
