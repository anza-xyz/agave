//! Batched SHA-256 entry points used by Merkle tree construction.

use solana_sha256_hasher::hashv;

/// Level width below which batching stops paying for itself.
pub const MIN_BATCHED_LEVEL: usize = 8;

/// Writes `sha256(prefix || bodies[i])` into `out[i]`.
///
/// # Panics
///
/// Panics if `bodies.len() != out.len()`.
pub fn hash_many_prefixed(prefix: &[u8], bodies: &[&[u8]], out: &mut [[u8; 32]]) {
    assert_eq!(bodies.len(), out.len());
    #[cfg(feature = "tape-sha256")]
    tape_sha256::hash_many_prefixed(prefix, bodies, out);
    #[cfg(not(feature = "tape-sha256"))]
    hash_many_prefixed_scalar(prefix, bodies, out);
}

/// Writes `sha256(prefix || left[i] || right[i])` into `out[i]`.
///
/// # Panics
///
/// Panics if `left`, `right` and `out` do not all have the same length.
pub fn hash_pairs(prefix: &[u8], left: &[&[u8]], right: &[&[u8]], out: &mut [[u8; 32]]) {
    assert_eq!(left.len(), right.len());
    assert_eq!(left.len(), out.len());
    #[cfg(feature = "tape-sha256")]
    tape_sha256::hash_pairs(prefix, left, right, out);
    #[cfg(not(feature = "tape-sha256"))]
    hash_pairs_scalar(prefix, left, right, out);
}

/// Reference implementation of [`hash_many_prefixed`]. Always available so that
/// accelerated backends can be differentially tested against it.
pub fn hash_many_prefixed_scalar(prefix: &[u8], bodies: &[&[u8]], out: &mut [[u8; 32]]) {
    assert_eq!(bodies.len(), out.len());
    for (body, out) in bodies.iter().zip(out) {
        out.copy_from_slice(hashv(&[prefix, body]).as_ref());
    }
}

/// Reference implementation of [`hash_pairs`]. Always available so that
/// accelerated backends can be differentially tested against it.
pub fn hash_pairs_scalar(prefix: &[u8], left: &[&[u8]], right: &[&[u8]], out: &mut [[u8; 32]]) {
    assert_eq!(left.len(), right.len());
    assert_eq!(left.len(), out.len());
    for ((left, right), out) in left.iter().zip(right).zip(out) {
        out.copy_from_slice(hashv(&[prefix, left, right]).as_ref());
    }
}

#[cfg(test)]
mod tests {
    use {super::*, rand::Rng, std::iter::repeat_with};

    // The prefixes the Merkle code actually passes in, plus degenerate ones.
    const PREFIXES: [&[u8]; 4] = [
        b"",
        b"\x01",
        b"\x00SOLANA_MERKLE_SHREDS_LEAF",
        b"\x01SOLANA_MERKLE_SHREDS_NODE",
    ];

    /// Body lengths chosen to straddle SHA-256 block boundaries once the
    /// prefix and the 9 bytes of length padding are added, since that is where
    /// a multi-block backend is most likely to disagree with the scalar one.
    fn body_lengths() -> Vec<usize> {
        let mut lengths: Vec<usize> = (0..200).collect();
        lengths.extend([255, 256, 511, 512, 987, 1019, 1024, 1025, 2048, 4096]);
        lengths
    }

    #[test]
    fn test_hash_many_prefixed_matches_hashv() {
        let mut rng = rand::rng();
        for prefix in PREFIXES {
            for len in body_lengths() {
                let bodies: Vec<Vec<u8>> = (0..17)
                    .map(|_| repeat_with(|| rng.random()).take(len).collect())
                    .collect();
                let bodies: Vec<&[u8]> = bodies.iter().map(Vec::as_slice).collect();
                // Every batch width from empty through past one full lane
                // group, so partially filled groups are covered too.
                for width in 0..=bodies.len() {
                    let bodies = &bodies[..width];
                    let mut got = vec![[0u8; 32]; width];
                    let mut want = vec![[0u8; 32]; width];
                    hash_many_prefixed(prefix, bodies, &mut got);
                    hash_many_prefixed_scalar(prefix, bodies, &mut want);
                    assert_eq!(got, want, "prefix {prefix:?} len {len} width {width}");
                    for (body, want) in bodies.iter().zip(&want) {
                        assert_eq!(hashv(&[prefix, body]).as_ref(), want);
                    }
                }
            }
        }
    }

    #[test]
    fn test_hash_pairs_matches_hashv() {
        let mut rng = rand::rng();
        // The Merkle tree only ever pairs 20-byte proof entries, but the seam
        // is not specialized to that, so exercise other widths as well.
        for prefix in PREFIXES {
            for len in [0usize, 1, 16, 19, 20, 21, 31, 32, 64, 128] {
                let left: Vec<Vec<u8>> = (0..17)
                    .map(|_| repeat_with(|| rng.random()).take(len).collect())
                    .collect();
                let right: Vec<Vec<u8>> = (0..17)
                    .map(|_| repeat_with(|| rng.random()).take(len).collect())
                    .collect();
                let left: Vec<&[u8]> = left.iter().map(Vec::as_slice).collect();
                let right: Vec<&[u8]> = right.iter().map(Vec::as_slice).collect();
                for width in 0..=left.len() {
                    let (left, right) = (&left[..width], &right[..width]);
                    let mut got = vec![[0u8; 32]; width];
                    let mut want = vec![[0u8; 32]; width];
                    hash_pairs(prefix, left, right, &mut got);
                    hash_pairs_scalar(prefix, left, right, &mut want);
                    assert_eq!(got, want, "prefix {prefix:?} len {len} width {width}");
                    for ((left, right), want) in left.iter().zip(right).zip(&want) {
                        assert_eq!(hashv(&[prefix, left, right]).as_ref(), want);
                    }
                }
            }
        }
    }

    #[test]
    fn test_ragged_body_lengths() {
        // Lanes finish at different block counts within one batch, which is the
        // case a fixed-width kernel is most likely to get wrong.
        let mut rng = rand::rng();
        let prefix = b"\x00SOLANA_MERKLE_SHREDS_LEAF";
        for _ in 0..64 {
            let bodies: Vec<Vec<u8>> = (0..33)
                .map(|_| {
                    let len = rng.random_range(0..3000);
                    repeat_with(|| rng.random()).take(len).collect()
                })
                .collect();
            let bodies: Vec<&[u8]> = bodies.iter().map(Vec::as_slice).collect();
            let mut got = vec![[0u8; 32]; bodies.len()];
            let mut want = vec![[0u8; 32]; bodies.len()];
            hash_many_prefixed(prefix, &bodies, &mut got);
            hash_many_prefixed_scalar(prefix, &bodies, &mut want);
            assert_eq!(got, want);
        }
    }
}
