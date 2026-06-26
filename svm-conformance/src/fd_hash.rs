//! Hashing used to summarize binary blobs (e.g. ELF sections, serialized VM
//! memory) for conformance comparisons.

use xxhash_rust::xxh64::xxh64;

pub fn fd_hash_without_seed(buf: &[u8]) -> u64 {
    fd_hash(0, buf)
}

pub fn fd_hash_u64_without_seed(buf: &[u64]) -> u64 {
    let bytes = unsafe {
        std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), std::mem::size_of_val(buf))
    };
    fd_hash(0, bytes)
}

pub fn fd_hash(seed: u64, buf: &[u8]) -> u64 {
    xxh64(buf, seed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fd_hash_matches_xxh64() {
        // Reference values from Firedancer's `fd_hash` (XXH64). The empty-input,
        // seed-0 case is the canonical XXH64 test vector.
        let long: Vec<u8> = (0u8..100).collect();
        assert_eq!(fd_hash(0, &[]), 0xef46db3751d8e999);
        assert_eq!(fd_hash(0, b"hello world"), 0x45ab6734b21e6968);
        assert_eq!(fd_hash(42, b"hello world"), 0x69c2b68f9d9352a1);
        assert_eq!(fd_hash(0, &long), 0x6ac1e58032166597);
    }
}
