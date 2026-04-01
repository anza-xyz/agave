//! SHA Syscall test

use {
    solana_msg::msg,
    solana_program_entrypoint::{custom_heap_default, custom_panic_default},
};

fn test_sha256_hasher() {
    use solana_sha256_hasher::hashv;
    let vals = &["Gaggablaghblagh!".as_ref(), "flurbos".as_ref()];

    let expected = solana_hash::Hash::new_from_array([
        0x9f, 0xa2, 0x7e, 0x8f, 0x7b, 0xc1, 0xec, 0xe8, 0xae, 0x7b, 0x9a, 0x91, 0x46, 0x53, 0x20,
        0x0f, 0x1c, 0x22, 0x8e, 0x56, 0x10, 0x30, 0x59, 0xfd, 0x35, 0x8d, 0x57, 0x54, 0x96, 0x47,
        0x2c, 0xc9,
    ]);

    assert_eq!(hashv(vals), expected);
}

fn test_keccak256_hasher() {
    use solana_keccak_hasher::hashv;
    let vals = &["Gaggablaghblagh!".as_ref(), "flurbos".as_ref()];

    let expected = solana_hash::Hash::new_from_array([
        0xd1, 0x9a, 0x9d, 0xe2, 0x89, 0x7f, 0x7c, 0x9e, 0x5, 0x32, 0x32, 0x22, 0xe8, 0xc6, 0xb4,
        0x88, 0x6b, 0x5b, 0xbb, 0xec, 0xd4, 0x42, 0xfd, 0x10, 0x7d, 0xd5, 0x9a, 0x6f, 0x21, 0xd3,
        0xb8, 0xa7,
    ]);

    assert_eq!(hashv(vals), expected);
}

fn test_blake3_hasher() {
    use solana_blake3_hasher::hashv;
    let v0: &[u8] = b"Gaggablaghblagh!";
    let v1: &[u8] = b"flurbos!";
    let vals: &[&[u8]] = &[v0, v1];
    let hash = blake3::hash(&[v0, v1].concat());
    assert_eq!(hashv(vals).as_bytes(), hash.as_bytes());
}

#[unsafe(no_mangle)]
pub extern "C" fn entrypoint(_input: *mut u8) -> u64 {
    msg!("sha");

    test_sha256_hasher();
    test_keccak256_hasher();
    test_blake3_hasher();

    0
}

custom_heap_default!();
custom_panic_default!();

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_sha() {
        test_sha256_hasher();
        test_keccak256_hasher();
        test_blake3_hasher();
    }
}
