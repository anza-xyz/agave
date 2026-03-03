//! SHA Syscall test

use {
    solana_msg::msg,
    solana_program_entrypoint::{custom_heap_default, custom_panic_default},
};

fn test_sha256_hasher() {
    use solana_sha256_hasher::hashv;
    let vals = &["Gaggablaghblagh!".as_ref(), "flurbos".as_ref()];
    #[cfg(target_os = "solana")]
    let hash = {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::default();
        for val in vals {
            hasher.update(val);
        }
        solana_hash::Hash::new_from_array(hasher.finalize().into())
    };
    #[cfg(not(target_os = "solana"))]
    let hash = {
        let mut hasher = solana_sha256_hasher::Hasher::default();
        hasher.hashv(vals);
        hasher.result()
    };
    assert_eq!(hashv(vals), hash);
}

fn test_keccak256_hasher() {
    use solana_keccak_hasher::hashv;
    let vals = &["Gaggablaghblagh!".as_ref(), "flurbos".as_ref()];
    #[cfg(target_os = "solana")]
    let hash = {
        use sha3::Digest;
        let mut hasher = sha3::Keccak256::default();
        for val in vals {
            hasher.update(val);
        }
        solana_hash::Hash::new_from_array(hasher.finalize().into())
    };
    #[cfg(not(target_os = "solana"))]
    let hash = {
        let mut hasher = solana_keccak_hasher::Hasher::default();
        hasher.hashv(vals);
        hasher.result()
    };
    assert_eq!(hashv(vals), hash);
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
