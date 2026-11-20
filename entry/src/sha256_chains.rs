//! Multi-lane SHA-256 for PoH hash chains.
//!
//! A PoH chain step is `h = sha256(h)` on a 32-byte `h`: always a single
//! 64-byte block whose second half is constant padding. The digest words of one
//! step are the message words of the next, so a chain can stay in vector
//! registers in the CPU's native SHA-256 state layout for many iterations.
//! Advancing `N` independent chains together interleaves their SHA-256 rounds
//! inside one function, so the core can overlap the chains' dependency chains
//! (a single chain is bound by SHA instruction latency).
//!
//! The round and message-schedule sequences are adapted from the `sha2` crate's
//! `aarch64` (from mbedtls) and `x86` SHA-NI backends.

/// SHA-256 initial hash value.
const IV: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// SHA-256 round constants.
const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Message words 8..16 of a 32-byte message: the `0x80` terminator and the
/// 256-bit message length.
const PAD: [u32; 8] = [0x8000_0000, 0, 0, 0, 0, 0, 0, 256];

fn to_words(bytes: [u8; 32]) -> [u32; 8] {
    let (chunks, _) = bytes.as_chunks::<4>();
    std::array::from_fn(|i| u32::from_be_bytes(chunks[i]))
}

fn to_bytes(words: [u32; 8]) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    for (chunk, word) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(words) {
        *chunk = word.to_be_bytes();
    }
    bytes
}

/// Applies `h = sha256(h)` `iterations` times to every lane, with all lanes'
/// rounds interleaved, using the CPU's SHA-256 instructions.
///
/// Returns `false`, leaving `hashes` untouched, when the CPU (or target) has no
/// supported SHA-256 instructions; the caller then uses the scalar hasher.
pub(crate) fn hash_chains<const N: usize>(hashes: &mut [[u8; 32]; N], iterations: u64) -> bool {
    #[cfg(target_arch = "aarch64")]
    {
        if std::arch::is_aarch64_feature_detected!("sha2") {
            let mut words = hashes.map(to_words);
            // SAFETY: the `sha2` feature was detected at runtime.
            unsafe { aarch64::hash_chains(&mut words, iterations) };
            *hashes = words.map(to_bytes);
            return true;
        }
    }
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if is_x86_feature_detected!("sha")
            && is_x86_feature_detected!("sse2")
            && is_x86_feature_detected!("ssse3")
            && is_x86_feature_detected!("sse4.1")
        {
            let mut words = hashes.map(to_words);
            // SAFETY: the `sha`, `sse2`, `ssse3` and `sse4.1` features were
            // detected at runtime.
            unsafe { x86::hash_chains(&mut words, iterations) };
            *hashes = words.map(to_bytes);
            return true;
        }
    }
    let _ = (hashes, iterations);
    false
}

#[cfg(target_arch = "aarch64")]
mod aarch64 {
    use {
        super::{IV, K, PAD},
        core::arch::aarch64::*,
    };

    /// Four rounds on every lane.
    #[inline]
    #[target_feature(enable = "sha2")]
    fn rounds4<const N: usize>(
        abcd: &mut [uint32x4_t; N],
        efgh: &mut [uint32x4_t; N],
        w: &[uint32x4_t; N],
        k: uint32x4_t,
    ) {
        for i in 0..N {
            let wk = vaddq_u32(w[i], k);
            let abcd_prev = abcd[i];
            abcd[i] = vsha256hq_u32(abcd_prev, efgh[i], wk);
            efgh[i] = vsha256h2q_u32(efgh[i], abcd_prev, wk);
        }
    }

    /// Next four message words on every lane: `w0 = su1(su0(w0, w1), w2, w3)`.
    #[inline]
    #[target_feature(enable = "sha2")]
    fn schedule<const N: usize>(
        w0: &mut [uint32x4_t; N],
        w1: &[uint32x4_t; N],
        w2: &[uint32x4_t; N],
        w3: &[uint32x4_t; N],
    ) {
        for i in 0..N {
            w0[i] = vsha256su1q_u32(vsha256su0q_u32(w0[i], w1[i]), w2[i], w3[i]);
        }
    }

    /// # Safety
    ///
    /// The CPU must support the `sha2` feature.
    #[target_feature(enable = "sha2")]
    pub(super) unsafe fn hash_chains<const N: usize>(words: &mut [[u32; 8]; N], iterations: u64) {
        // SAFETY: every pointer below points at 4 readable/writable `u32`s.
        unsafe {
            let iv_abcd = vld1q_u32(IV.as_ptr());
            let iv_efgh = vld1q_u32(IV.as_ptr().add(4));
            let pad_lo = vld1q_u32(PAD.as_ptr());
            let pad_hi = vld1q_u32(PAD.as_ptr().add(4));
            let mut k = [vdupq_n_u32(0); 16];
            for (i, k) in k.iter_mut().enumerate() {
                *k = vld1q_u32(K.as_ptr().add(4 * i));
            }
            // Current hash of every lane: words 0..4 and 4..8, which are also
            // the next step's message words 0..8.
            let mut lo = [vdupq_n_u32(0); N];
            let mut hi = [vdupq_n_u32(0); N];
            for i in 0..N {
                lo[i] = vld1q_u32(words[i].as_ptr());
                hi[i] = vld1q_u32(words[i].as_ptr().add(4));
            }

            for _ in 0..iterations {
                let mut abcd = [iv_abcd; N];
                let mut efgh = [iv_efgh; N];
                let mut s0 = lo;
                let mut s1 = hi;
                let mut s2 = [pad_lo; N];
                let mut s3 = [pad_hi; N];

                rounds4(&mut abcd, &mut efgh, &s0, k[0]);
                rounds4(&mut abcd, &mut efgh, &s1, k[1]);
                rounds4(&mut abcd, &mut efgh, &s2, k[2]);
                rounds4(&mut abcd, &mut efgh, &s3, k[3]);
                for t in (4..16).step_by(4) {
                    schedule(&mut s0, &s1, &s2, &s3);
                    rounds4(&mut abcd, &mut efgh, &s0, k[t]);
                    schedule(&mut s1, &s2, &s3, &s0);
                    rounds4(&mut abcd, &mut efgh, &s1, k[t + 1]);
                    schedule(&mut s2, &s3, &s0, &s1);
                    rounds4(&mut abcd, &mut efgh, &s2, k[t + 2]);
                    schedule(&mut s3, &s0, &s1, &s2);
                    rounds4(&mut abcd, &mut efgh, &s3, k[t + 3]);
                }

                for i in 0..N {
                    lo[i] = vaddq_u32(abcd[i], iv_abcd);
                    hi[i] = vaddq_u32(efgh[i], iv_efgh);
                }
            }

            for i in 0..N {
                vst1q_u32(words[i].as_mut_ptr(), lo[i]);
                vst1q_u32(words[i].as_mut_ptr().add(4), hi[i]);
            }
        }
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
mod x86 {
    use super::{IV, K, PAD};
    #[cfg(target_arch = "x86")]
    use core::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::*;

    /// Four rounds on every lane, with the state in SHA-NI's ABEF/CDGH layout.
    #[inline]
    #[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
    fn rounds4<const N: usize>(
        abef: &mut [__m128i; N],
        cdgh: &mut [__m128i; N],
        w: &[__m128i; N],
        k: __m128i,
    ) {
        for i in 0..N {
            let wk = _mm_add_epi32(w[i], k);
            cdgh[i] = _mm_sha256rnds2_epu32(cdgh[i], abef[i], wk);
            let wk_hi = _mm_shuffle_epi32(wk, 0x0E);
            abef[i] = _mm_sha256rnds2_epu32(abef[i], cdgh[i], wk_hi);
        }
    }

    /// Next four message words on every lane from the previous sixteen.
    #[inline]
    #[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
    fn schedule<const N: usize>(
        w0: &[__m128i; N],
        w1: &[__m128i; N],
        w2: &[__m128i; N],
        w3: &[__m128i; N],
    ) -> [__m128i; N] {
        let mut out = *w0;
        for i in 0..N {
            let t1 = _mm_sha256msg1_epu32(w0[i], w1[i]);
            let t2 = _mm_alignr_epi8(w3[i], w2[i], 4);
            out[i] = _mm_sha256msg2_epu32(_mm_add_epi32(t1, t2), w3[i]);
        }
        out
    }

    /// `[a, b, c, d]`, `[e, f, g, h]` (lane 0 first) to SHA-NI's ABEF/CDGH.
    #[inline]
    #[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
    fn to_abef_cdgh(abcd: __m128i, efgh: __m128i) -> (__m128i, __m128i) {
        let cdab = _mm_shuffle_epi32(abcd, 0xB1);
        let hgfe = _mm_shuffle_epi32(efgh, 0x1B);
        (
            _mm_alignr_epi8(cdab, hgfe, 8),
            _mm_blend_epi16(hgfe, cdab, 0xF0),
        )
    }

    /// Inverse of `to_abef_cdgh`.
    #[inline]
    #[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
    fn from_abef_cdgh(abef: __m128i, cdgh: __m128i) -> (__m128i, __m128i) {
        let feba = _mm_shuffle_epi32(abef, 0x1B);
        let dchg = _mm_shuffle_epi32(cdgh, 0xB1);
        (
            _mm_blend_epi16(feba, dchg, 0xF0),
            _mm_alignr_epi8(dchg, feba, 8),
        )
    }

    /// # Safety
    ///
    /// The CPU must support the `sha`, `sse2`, `ssse3` and `sse4.1` features.
    #[allow(clippy::cast_ptr_alignment)] // unaligned loads and stores
    #[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
    pub(super) unsafe fn hash_chains<const N: usize>(words: &mut [[u32; 8]; N], iterations: u64) {
        // SAFETY: every pointer below points at 4 readable/writable `u32`s and
        // is only used with unaligned loads and stores.
        unsafe {
            let load = |p: *const u32| _mm_loadu_si128(p.cast::<__m128i>());
            let (iv_abef, iv_cdgh) = to_abef_cdgh(load(IV.as_ptr()), load(IV.as_ptr().add(4)));
            let pad_lo = load(PAD.as_ptr());
            let pad_hi = load(PAD.as_ptr().add(4));
            let mut k = [_mm_setzero_si128(); 16];
            for (i, k) in k.iter_mut().enumerate() {
                *k = load(K.as_ptr().add(4 * i));
            }
            // Current hash of every lane as `[a, b, c, d]`, `[e, f, g, h]`,
            // which are also the next step's message words 0..8.
            let mut lo = [_mm_setzero_si128(); N];
            let mut hi = [_mm_setzero_si128(); N];
            for i in 0..N {
                lo[i] = load(words[i].as_ptr());
                hi[i] = load(words[i].as_ptr().add(4));
            }

            for _ in 0..iterations {
                let mut abef = [iv_abef; N];
                let mut cdgh = [iv_cdgh; N];
                let mut w0 = lo;
                let mut w1 = hi;
                let mut w2 = [pad_lo; N];
                let mut w3 = [pad_hi; N];
                let mut w4;

                rounds4(&mut abef, &mut cdgh, &w0, k[0]);
                rounds4(&mut abef, &mut cdgh, &w1, k[1]);
                rounds4(&mut abef, &mut cdgh, &w2, k[2]);
                rounds4(&mut abef, &mut cdgh, &w3, k[3]);
                w4 = schedule(&w0, &w1, &w2, &w3);
                rounds4(&mut abef, &mut cdgh, &w4, k[4]);
                w0 = schedule(&w1, &w2, &w3, &w4);
                rounds4(&mut abef, &mut cdgh, &w0, k[5]);
                w1 = schedule(&w2, &w3, &w4, &w0);
                rounds4(&mut abef, &mut cdgh, &w1, k[6]);
                w2 = schedule(&w3, &w4, &w0, &w1);
                rounds4(&mut abef, &mut cdgh, &w2, k[7]);
                w3 = schedule(&w4, &w0, &w1, &w2);
                rounds4(&mut abef, &mut cdgh, &w3, k[8]);
                w4 = schedule(&w0, &w1, &w2, &w3);
                rounds4(&mut abef, &mut cdgh, &w4, k[9]);
                w0 = schedule(&w1, &w2, &w3, &w4);
                rounds4(&mut abef, &mut cdgh, &w0, k[10]);
                w1 = schedule(&w2, &w3, &w4, &w0);
                rounds4(&mut abef, &mut cdgh, &w1, k[11]);
                w2 = schedule(&w3, &w4, &w0, &w1);
                rounds4(&mut abef, &mut cdgh, &w2, k[12]);
                w3 = schedule(&w4, &w0, &w1, &w2);
                rounds4(&mut abef, &mut cdgh, &w3, k[13]);
                w4 = schedule(&w0, &w1, &w2, &w3);
                rounds4(&mut abef, &mut cdgh, &w4, k[14]);
                w0 = schedule(&w1, &w2, &w3, &w4);
                rounds4(&mut abef, &mut cdgh, &w0, k[15]);

                for i in 0..N {
                    (lo[i], hi[i]) = from_abef_cdgh(
                        _mm_add_epi32(abef[i], iv_abef),
                        _mm_add_epi32(cdgh[i], iv_cdgh),
                    );
                }
            }

            for i in 0..N {
                _mm_storeu_si128(words[i].as_mut_ptr().cast::<__m128i>(), lo[i]);
                _mm_storeu_si128(words[i].as_mut_ptr().add(4).cast::<__m128i>(), hi[i]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, rand::Rng};

    fn scalar(mut hash: [u8; 32], iterations: u64) -> [u8; 32] {
        for _ in 0..iterations {
            hash = solana_sha256_hasher::hash(&hash).to_bytes();
        }
        hash
    }

    /// Whether this test run actually exercises an accelerated kernel.
    fn accelerated() -> bool {
        hash_chains(&mut [[0u8; 32]], 1)
    }

    #[test]
    fn test_hash_chains_known_answer() {
        // sha256 of 32 zero bytes.
        let expected: [u8; 32] = [
            0x66, 0x68, 0x7a, 0xad, 0xf8, 0x62, 0xbd, 0x77, 0x6c, 0x8f, 0xc1, 0x8b, 0x8e, 0x9f,
            0x8e, 0x20, 0x08, 0x97, 0x14, 0x85, 0x6e, 0xe2, 0x33, 0xb3, 0x90, 0x2a, 0x59, 0x1d,
            0x0d, 0x5f, 0x29, 0x25,
        ];
        assert_eq!(scalar([0; 32], 1), expected);
        let mut hashes = [[0u8; 32]];
        if hash_chains(&mut hashes, 1) {
            assert_eq!(hashes[0], expected);
        }
    }

    #[test]
    fn test_hash_chains_match_scalar() {
        fn check<const N: usize>() {
            let mut rng = rand::rng();
            for iterations in [0, 1, 2, 3, 7, 64, 1000] {
                let starts: [[u8; 32]; N] = std::array::from_fn(|_| rng.random());
                let mut hashes = starts;
                if !hash_chains(&mut hashes, iterations) {
                    assert_eq!(hashes, starts);
                    continue;
                }
                for (hash, start) in hashes.iter().zip(starts) {
                    assert_eq!(*hash, scalar(start, iterations));
                }
            }
        }
        check::<1>();
        check::<2>();
        check::<3>();
        check::<4>();
        check::<8>();
    }

    #[test]
    fn test_hash_chains_lanes_are_independent() {
        // Identical lanes stay identical; one different lane changes only itself.
        let mut hashes = [[7u8; 32]; 4];
        hashes[2][0] ^= 1;
        let starts = hashes;
        if hash_chains(&mut hashes, 5) {
            assert_eq!(hashes[0], hashes[1]);
            assert_eq!(hashes[0], hashes[3]);
            assert_eq!(hashes[0], scalar(starts[0], 5));
            assert_eq!(hashes[2], scalar(starts[2], 5));
        }
    }

    #[test]
    fn test_hash_chains_accelerated_where_expected() {
        // Every Apple silicon CPU has the SHA-256 instructions, so there the
        // kernel (not the fallback) must be what the other tests exercised.
        if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
            assert!(accelerated());
        }
    }
}
