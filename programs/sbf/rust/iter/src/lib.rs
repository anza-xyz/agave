//! Example Rust-based SBF program tests loop iteration

#![allow(clippy::arithmetic_side_effects)]

use {
    solana_program::log::sol_log_64,
    solana_program_entrypoint::{SUCCESS, custom_heap_default, custom_panic_default},
};

#[no_mangle]
pub extern "C" fn entrypoint(_input: *mut u8) -> u64 {
    const ITERS: usize = 100;
    let ones = [1_u64; ITERS];
    let mut sum: u64 = 0;

    for v in ones.iter() {
        sum += *v;
    }
    sol_log_64(0xff, 0, 0, 0, sum);
    assert_eq!(sum, ITERS as u64);

    SUCCESS
}

custom_heap_default!();
custom_panic_default!();

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_entrypoint() {
        assert_eq!(SUCCESS, entrypoint(std::ptr::null_mut()));
    }
}
