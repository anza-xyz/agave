//! This module defines memory regions

use rand::Rng;
use std::ops::Range;

/// fuzzing utility function
pub fn fuzz<F>(
    bytes: &[u8],
    outer_iters: usize,
    inner_iters: usize,
    offset: Range<usize>,
    value: Range<u8>,
    work: F,
) where
    F: Fn(&mut [u8]),
{
    let mut rng = rand::thread_rng();
    for _ in 0..outer_iters {
        let mut mangled_bytes = bytes.to_vec();
        for _ in 0..inner_iters {
            let offset = rng.gen_range(offset.start..offset.end);
            let value = rng.gen_range(value.start..value.end);
            mangled_bytes[offset] = value;
            work(&mut mangled_bytes);
        }
    }
}
