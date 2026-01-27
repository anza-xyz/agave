//! Test builtin mem functions

#![cfg(target_os = "solana")]
#![feature(rustc_private)]

extern crate compiler_builtins;
use {
    solana_program_entrypoint::{SUCCESS, custom_heap_default, custom_panic_default},
    solana_sbf_rust_mem_dep::{MemOps, run_mem_tests},
};

#[no_mangle]
pub extern "C" fn entrypoint(_input: *mut u8) -> u64 {
    #[derive(Default)]
    struct MemOpSyscalls();
    impl MemOps for MemOpSyscalls {
        unsafe fn memcpy(&self, dst: &mut [u8], src: &[u8], n: usize) {
            unsafe {
                compiler_builtins::mem::memcpy(dst.as_mut_ptr(), src.as_ptr(), n);
            }
        }
        unsafe fn memmove(&self, dst: *mut u8, src: *mut u8, n: usize) {
            compiler_builtins::mem::memmove(dst, src, n);
        }
        unsafe fn memset(&self, s: &mut [u8], c: u8, n: usize) {
            unsafe {
                compiler_builtins::mem::memset(s.as_mut_ptr(), c as i32, n);
            }
        }
        unsafe fn memcmp(&self, s1: &[u8], s2: &[u8], n: usize) -> i32 {
            unsafe { compiler_builtins::mem::memcmp(s1.as_ptr(), s2.as_ptr(), n) }
        }
    }
    let mem_ops = MemOpSyscalls::default();

    run_mem_tests(mem_ops);

    SUCCESS
}

custom_heap_default!();
custom_panic_default!();
