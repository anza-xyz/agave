//! Emergency Abort Program
//! Based on: https://github.com/deanmlittle/sbpf-asm-abort
//! Author: deanmlittle
//! 
//! This program simply loads 1 into r0 and exits, causing all transactions to fail.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

/// Entry point for the emergency abort program
#[no_mangle]
pub extern "C" fn entrypoint(_input: *mut u8) -> u64 {
    // Load 1 into r0 and exit - this causes all transactions to fail
    unsafe {
        core::arch::asm!(
            "lddw r0, 1",
            "exit",
            options(noreturn)
        );
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}
