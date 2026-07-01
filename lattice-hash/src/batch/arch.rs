//! Stable `core::arch` SIMD backends for the batched BLAKE3 lattice-hash kernel.
//!
//! Provenance: the overall structure is ported from firedancer's batched BLAKE3
//! (`src/ballet/blake3`). The across-lanes input compression with per-lane length
//! masking mirrors `fd_blake3_avx512_compress16` + `fd_blake3_fini_xof_compress`,
//! and the 2048-byte XOF expansion mirrors `fd_blake3_fini_2048` (32 root-block
//! compressions with an incrementing counter). Fusing all lanes' group-add into
//! one `LtHash` is firedancer's `fd_blake3_lthash_batch{8,16}` idea (`fd_blake3.c`).
//! The BLAKE3 compression itself (`compress_pre`, `g`, `round`, the message
//! schedule) mirrors the upstream `blake3` crate's portable reference
//! (`src/portable.rs`), and the per-lane vector ops behind [`Lanes`] mirror
//! firedancer's `wu_`/`wwu_` SIMD wrappers (`src/util/simd`).
//!
//! Two pieces are ours, in neither source: the [`Lanes`] trait, which lets one
//! generic kernel body serve both AVX2 and AVX512, and the in-register SWAR `u16`
//! reduction ([`swar_add_u16`]) — firedancer's `fd_lthash_add` group-adds with a
//! scalar element loop, not in vectors.
//!
//! The per-lane vector ops are abstracted behind the [`Lanes`] trait; the kernel
//! body (input-compress phase, XOF phase, transpose-load, SWAR-u16 reduce) is
//! written once, generic over `Lanes`.  Backends: AVX2 (`__m256i`, 8 lanes) and
//! AVX512 (`__m512i`, 16 lanes).
//!
//! Each backend is checked against the `blake3` crate in this module's `tests`
//! (whichever backends the test CPU supports).
#![cfg(target_arch = "x86_64")]
// A hash kernel: the arithmetic is intentional (wrapping hash adds) or bounded
// index/offset math, not a place where overflow checks add value, as poh/xdp do.
#![allow(clippy::arithmetic_side_effects)]

use {
    super::{BLOCK_LEN, MAX_LANES},
    crate::lt_hash::LtHash,
    core::arch::x86_64::*,
};

// BLAKE3 protocol constants used only by this SIMD kernel. Defined here (rather
// than in the always-compiled parent) so they don't become dead code on targets
// without an x86 backend.
/// Bytes of XOF output per message: exactly one `LtHash` (1024 little-endian u16).
const XOF_LEN: usize = size_of::<LtHash>();
/// Compression blocks needed to fill one XOF output (`XOF_LEN / BLOCK_LEN` = 32).
const NUM_XOF_BLOCKS: usize = XOF_LEN / BLOCK_LEN;
/// `u32` words in one 64-byte BLAKE3 block / compression state (`BLOCK_LEN / 4` = 16).
const NUM_BLOCK_WORDS: usize = BLOCK_LEN / size_of::<u32>();
/// `u32` words in a BLAKE3 chaining value — the IV length and half the 16-word
/// state (256 bits = 8).
const NUM_CV_WORDS: usize = 8;

// BLAKE3 domain-separation flags.
const CHUNK_START: u32 = 1 << 0;
const CHUNK_END: u32 = 1 << 1;
const ROOT: u32 = 1 << 3;

/// BLAKE3 initialization vector.
const IV: [u32; NUM_CV_WORDS] = [
    0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A, 0x510E527F, 0x9B05688C, 0x1F83D9AB, 0x5BE0CD19,
];

/// BLAKE3 7-round message word permutation schedule (standard, matches
/// firedancer's `FD_BLAKE3_MSG_SCHEDULE` and the upstream reference).
const MSG_SCHEDULE: [[usize; NUM_BLOCK_WORDS]; 7] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8],
    [3, 4, 10, 12, 13, 2, 7, 14, 6, 5, 9, 0, 11, 15, 8, 1],
    [10, 7, 12, 9, 14, 3, 13, 15, 4, 0, 11, 2, 5, 8, 1, 6],
    [12, 13, 9, 11, 15, 10, 14, 8, 7, 2, 5, 3, 0, 1, 6, 4],
    [9, 14, 11, 5, 8, 12, 15, 1, 13, 3, 0, 10, 2, 6, 4, 7],
    [11, 15, 5, 0, 1, 9, 8, 6, 14, 10, 2, 12, 3, 4, 7, 13],
];

/// A vector of `N` parallel `u32` lanes (one per message), abstracting the
/// per-lane ops that firedancer spells out per backend in its `wu_` (AVX2) and
/// `wwu_` (AVX512) SIMD wrappers.  All methods are `#[target_feature]`-free here
/// and `#[inline(always)]`; they become legal + vectorized once monomorphized
/// inside a `#[target_feature]` wrapper.
///
/// # Safety
/// Implementors use SIMD intrinsics; every method must only be called from a
/// context where this backend's target feature is enabled (the runtime-detected
/// `run_*` wrappers guarantee this).
#[allow(clippy::missing_safety_doc)]
pub trait Lanes: Copy {
    /// Number of lanes (8 for AVX2, 16 for AVX512).
    const N: usize;
    /// Per-lane "active" selector used to blend the chaining value.
    type Mask: Copy;

    /// Broadcast one `u32` into all `N` lanes (every lane gets `x`).
    unsafe fn splat(x: u32) -> Self;
    /// Load `N` contiguous `u32` from `p`.
    unsafe fn load(p: *const u32) -> Self;
    /// Store `N` contiguous `u32` to `p`.
    unsafe fn store(self, p: *mut u32);
    unsafe fn add(self, o: Self) -> Self;
    unsafe fn xor(self, o: Self) -> Self;
    unsafe fn and(self, o: Self) -> Self;
    /// Rotate each lane right by `R` bits (`R` in {16,12,8,7}).
    unsafe fn ror<const R: u32>(self) -> Self;
    /// Build a mask from per-lane active flags (`active.len() == N`).
    unsafe fn mask(active: &[bool]) -> Self::Mask;
    /// Per lane: `if_active` where the mask is set, else `if_inactive`.
    unsafe fn select(m: Self::Mask, if_active: Self, if_inactive: Self) -> Self;
    /// In-place `N x N` element transpose of `io` (`io.len() == N`).
    unsafe fn transpose(io: &mut [Self]);
}

/// Vectorized BLAKE3 quarter-round `g` and full `round`, identical in structure
/// to the `blake3` crate's `g`/`round` (`src/portable.rs`) and firedancer's
/// `round_fn{8,16}` — only the element type differs (a `Lanes` vector instead of
/// a scalar `u32`).
#[inline(always)]
#[allow(clippy::too_many_arguments)]
unsafe fn g<L: Lanes>(
    v: &mut [L; NUM_BLOCK_WORDS],
    a: usize,
    b: usize,
    c: usize,
    d: usize,
    mx: L,
    my: L,
) {
    unsafe {
        v[a] = v[a].add(v[b]).add(mx);
        v[d] = v[d].xor(v[a]).ror::<16>();
        v[c] = v[c].add(v[d]);
        v[b] = v[b].xor(v[c]).ror::<12>();
        v[a] = v[a].add(v[b]).add(my);
        v[d] = v[d].xor(v[a]).ror::<8>();
        v[c] = v[c].add(v[d]);
        v[b] = v[b].xor(v[c]).ror::<7>();
    }
}

#[inline(always)]
unsafe fn round<L: Lanes>(v: &mut [L; NUM_BLOCK_WORDS], m: &[L; NUM_BLOCK_WORDS], r: usize) {
    let s = &MSG_SCHEDULE[r];
    unsafe {
        g(v, 0, 4, 8, 12, m[s[0]], m[s[1]]);
        g(v, 1, 5, 9, 13, m[s[2]], m[s[3]]);
        g(v, 2, 6, 10, 14, m[s[4]], m[s[5]]);
        g(v, 3, 7, 11, 15, m[s[6]], m[s[7]]);
        g(v, 0, 5, 10, 15, m[s[8]], m[s[9]]);
        g(v, 1, 6, 11, 12, m[s[10]], m[s[11]]);
        g(v, 2, 7, 8, 13, m[s[12]], m[s[13]]);
        g(v, 3, 4, 9, 14, m[s[14]], m[s[15]]);
    }
}

/// SWAR add of two lane-vectors as independent wrapping `u16` fields:
/// `(a & 0x7fff7fff) + (b & 0x7fff7fff)) ^ ((a ^ b) & 0x80008000)` — sum the low
/// 15 bits of each `u16` without carrying across the 16-bit boundary, then patch
/// the top bit back in via xor. New code (our vectorized LtHash group-add);
/// firedancer's `fd_lthash_add` (`fd_lthash.h`) instead loops over the 1024
/// elements with scalar `u16` adds.
#[inline(always)]
unsafe fn swar_add_u16<L: Lanes>(a: L, b: L) -> L {
    unsafe {
        let lo = L::splat(0x7fff_7fff);
        let hi = L::splat(0x8000_8000);
        a.and(lo).add(b.and(lo)).xor(a.xor(b).and(hi))
    }
}

/// Build the 16-word BLAKE3 state `[cv, IV[0..4], counter_lo, counter_hi,
/// block_len, flags]` and run the 7 rounds, returning the post-round state `v`
/// *before* the feed-forward. The vector analogue of the `blake3` crate's
/// `compress_pre` (`src/portable.rs`); callers apply the feed-forward themselves
/// — the low 8 words for a chaining value (blake3's `compress_in_place`) or all
/// 16 for XOF output (blake3's `compress_xof`).
#[inline(always)]
#[allow(clippy::too_many_arguments)]
unsafe fn compress_pre<L: Lanes>(
    cv: &[L; NUM_CV_WORDS],
    iv: &[L; NUM_CV_WORDS],
    m: &[L; NUM_BLOCK_WORDS],
    counter_lo: L,
    counter_hi: L,
    block_len: L,
    flags: L,
) -> [L; NUM_BLOCK_WORDS] {
    let mut v: [L; NUM_BLOCK_WORDS] = [
        cv[0], cv[1], cv[2], cv[3], cv[4], cv[5], cv[6], cv[7], iv[0], iv[1], iv[2], iv[3],
        counter_lo, counter_hi, block_len, flags,
    ];
    unsafe {
        for r in 0..7 {
            round(&mut v, m, r);
        }
    }
    v
}

/// Fused batch of `L::N` messages: hash each to a 2048-byte XOF and group-add
/// (wrapping `u16`) all of them into `acc`. Reads each lane's message in place
/// from a caller-owned, zero-padded buffer (no copy / staging here). The Rust
/// analogue of firedancer's `fd_blake3_lthash_batch{8,16}`, generic over the
/// lane width.
///
/// # Safety
/// Must be called from a context where `L`'s target feature is enabled.
/// `bufs.len()` and `lens.len()` must both equal `L::N`. Each `bufs[l]` must be
/// zero-padded to its block boundary and hold at least `ceil(lens[l]/64)*16`
/// `u32` words (i.e. `ceil(lens[l]/64)` full 64-byte blocks).
#[inline(always)]
unsafe fn compress_batch<L: Lanes>(bufs: &[&[u32]], lens: &[usize], acc: &mut LtHash) {
    let n = L::N;
    debug_assert_eq!(bufs.len(), n);
    debug_assert_eq!(lens.len(), n);
    let sub_blocks = NUM_BLOCK_WORDS / n; // AVX2: 2 tiles of 8 words; AVX512: 1 tile of 16

    let zero = unsafe { L::splat(0) };
    let iv: [L; NUM_CV_WORDS] = core::array::from_fn(|w| unsafe { L::splat(IV[w]) });
    let mut h = iv;
    let mut off = [0usize; MAX_LANES];

    // Build a lane vector from up to `N` per-lane u32 values (rest zero).
    let lanes = |vals: &[u32]| -> L {
        let mut t = [0u32; MAX_LANES];
        t[..vals.len()].copy_from_slice(vals);
        unsafe { L::load(t.as_ptr()) }
    };

    // Load the current 64-byte block of every lane into SoA word vectors,
    // reading directly from the caller's zero-padded buffers.
    let load_m = |off: &[usize; MAX_LANES]| -> [L; NUM_BLOCK_WORDS] {
        let mut m = [zero; NUM_BLOCK_WORDS];
        for blk in 0..sub_blocks {
            let mut rows = [zero; MAX_LANES];
            for l in 0..n {
                // `off[l]` is a byte offset; `bufs[l]` is `&[u32]`
                let base = off[l] / size_of::<u32>() + blk * n;
                rows[l] = unsafe { L::load(bufs[l].as_ptr().add(base)) };
            }
            unsafe { L::transpose(&mut rows[..n]) };
            m[blk * n..blk * n + n].copy_from_slice(&rows[..n]);
        }
        m
    };

    // ---- Input compression phase (mirrors fd_blake3_fini_xof_compress) ----
    // Compress every block but the root, chaining the CV forward. Per-lane flags
    // + an active-lane mask handle messages of differing length within the batch,
    // as in fd_blake3_avx512_compress16. Loop ends holding the root block (the
    // last block of each lane), which the XOF phase compresses repeatedly.
    let (root_m, root_sz, root_flags) = loop {
        let mut block_flags = [0u32; MAX_LANES];
        let mut block_sz = [0u32; MAX_LANES];
        let mut active = [false; MAX_LANES];
        for l in 0..n {
            let is_last = lens[l] <= off[l] + BLOCK_LEN;
            let is_first = off[l] == 0;
            let mut f = if is_last { ROOT | CHUNK_END } else { 0 };
            if is_first {
                f |= CHUNK_START;
            }
            block_flags[l] = f;
            block_sz[l] = (lens[l] - off[l]).min(BLOCK_LEN) as u32;
            active[l] = !is_last;
        }

        let m = load_m(&off);
        let block_sz_v = lanes(&block_sz[..n]);
        let block_flags_v = lanes(&block_flags[..n]);

        if !active[..n].iter().any(|&a| a) {
            break (m, block_sz_v, block_flags_v);
        }

        // Compress this block (chunk counter is 0 for a single chunk), then fold
        // into the CV of active lanes only and advance them. The feed-forward
        // keeps just the low 8 words (the next chaining value) — the vector
        // analogue of blake3's `compress_in_place`.
        let v = unsafe { compress_pre(&h, &iv, &m, zero, zero, block_sz_v, block_flags_v) };
        unsafe {
            let mask = L::mask(&active[..n]);
            for w in 0..NUM_CV_WORDS {
                h[w] = L::select(mask, v[w].xor(v[w + NUM_CV_WORDS]), h[w]);
            }
        }
        for l in 0..n {
            if active[l] {
                off[l] += BLOCK_LEN;
            }
        }
    };

    // ---- XOF expansion phase (mirrors fd_blake3_fini_2048) ----
    // Re-compress the root block NUM_XOF_BLOCKS (32) times with an incrementing
    // output-block counter; each pass yields one 64-byte output block, which we
    // transpose back to per-lane rows and SWAR-u16-reduce into `acc`.
    let mut out_words = [zero; NUM_BLOCK_WORDS];
    let mut s_arr = [0u32; MAX_LANES];
    for xof_block in 0..NUM_XOF_BLOCKS {
        let counter = xof_block as u64;
        // XOF feed-forward keeps all 16 words (blake3's `compress_xof`):
        // out[w] = v[w]^v[w+8] for w<8, else h[w-8]^v[w].
        let v = unsafe {
            compress_pre(
                &h,
                &iv,
                &root_m,
                L::splat(counter as u32),
                L::splat((counter >> 32) as u32),
                root_sz,
                root_flags,
            )
        };
        unsafe {
            for w in 0..NUM_CV_WORDS {
                out_words[w] = v[w].xor(v[w + NUM_CV_WORDS]);
                out_words[w + NUM_CV_WORDS] = h[w].xor(v[w + NUM_CV_WORDS]);
            }
        }

        let base = xof_block * (BLOCK_LEN / 2); // u16 element index
        for blk in 0..sub_blocks {
            let mut cols = [zero; MAX_LANES];
            cols[..n].copy_from_slice(&out_words[blk * n..blk * n + n]);
            unsafe {
                L::transpose(&mut cols[..n]); // cols[l] = lane l's words
                let mut s = cols[0];
                for &col in &cols[1..n] {
                    s = swar_add_u16(s, col);
                }
                s.store(s_arr.as_mut_ptr());
            }
            for (j, packed) in s_arr[..n].iter().enumerate() {
                let e = base + (blk * n + j) * 2;
                acc.0[e] = acc.0[e].wrapping_add(*packed as u16);
                acc.0[e + 1] = acc.0[e + 1].wrapping_add((*packed >> 16) as u16);
            }
        }
    }
}

// ----------------------------- AVX2 backend -----------------------------

impl Lanes for __m256i {
    const N: usize = 8;
    type Mask = __m256i;

    #[inline(always)]
    unsafe fn splat(x: u32) -> Self {
        unsafe { _mm256_set1_epi32(x as i32) }
    }
    #[inline(always)]
    unsafe fn load(p: *const u32) -> Self {
        unsafe { _mm256_loadu_si256(p as *const __m256i) }
    }
    #[inline(always)]
    unsafe fn store(self, p: *mut u32) {
        unsafe { _mm256_storeu_si256(p as *mut __m256i, self) }
    }
    #[inline(always)]
    unsafe fn add(self, o: Self) -> Self {
        unsafe { _mm256_add_epi32(self, o) }
    }
    #[inline(always)]
    unsafe fn xor(self, o: Self) -> Self {
        unsafe { _mm256_xor_si256(self, o) }
    }
    #[inline(always)]
    unsafe fn and(self, o: Self) -> Self {
        unsafe { _mm256_and_si256(self, o) }
    }
    #[inline(always)]
    unsafe fn ror<const R: u32>(self) -> Self {
        // AVX2 has no vector rotate; use shift|shift, as the blake3 crate's
        // `rot16/12/8/7` (`src/rust_avx2.rs`) and firedancer's `wu_ror` do. The
        // `match` on the const `R` resolves to one arm with literal shift immediates.
        unsafe {
            match R {
                16 => _mm256_or_si256(_mm256_srli_epi32::<16>(self), _mm256_slli_epi32::<16>(self)),
                12 => _mm256_or_si256(_mm256_srli_epi32::<12>(self), _mm256_slli_epi32::<20>(self)),
                8 => _mm256_or_si256(_mm256_srli_epi32::<8>(self), _mm256_slli_epi32::<24>(self)),
                7 => _mm256_or_si256(_mm256_srli_epi32::<7>(self), _mm256_slli_epi32::<25>(self)),
                _ => unreachable!(),
            }
        }
    }
    #[inline(always)]
    unsafe fn mask(active: &[bool]) -> Self::Mask {
        // `active.len() == N == 8` per the trait contract, so fill all 8 lanes.
        let t: [i32; 8] = core::array::from_fn(|i| if active[i] { -1 } else { 0 });
        unsafe { _mm256_loadu_si256(t.as_ptr() as *const __m256i) }
    }
    #[inline(always)]
    unsafe fn select(m: Self::Mask, if_active: Self, if_inactive: Self) -> Self {
        unsafe { _mm256_blendv_epi8(if_inactive, if_active, m) }
    }
    #[inline(always)]
    unsafe fn transpose(io: &mut [Self]) {
        // Standard 8x8 u32 transpose: unpck32 -> unpck64 -> permute2x128.
        // Mirrors the blake3 crate's `transpose_vecs` (`src/rust_avx2.rs`).
        unsafe {
            let (a, b, c, d, e, f, g, h) = (io[0], io[1], io[2], io[3], io[4], io[5], io[6], io[7]);
            let t0 = _mm256_unpacklo_epi32(a, b);
            let t1 = _mm256_unpackhi_epi32(a, b);
            let t2 = _mm256_unpacklo_epi32(c, d);
            let t3 = _mm256_unpackhi_epi32(c, d);
            let t4 = _mm256_unpacklo_epi32(e, f);
            let t5 = _mm256_unpackhi_epi32(e, f);
            let t6 = _mm256_unpacklo_epi32(g, h);
            let t7 = _mm256_unpackhi_epi32(g, h);
            let u0 = _mm256_unpacklo_epi64(t0, t2);
            let u1 = _mm256_unpackhi_epi64(t0, t2);
            let u2 = _mm256_unpacklo_epi64(t1, t3);
            let u3 = _mm256_unpackhi_epi64(t1, t3);
            let u4 = _mm256_unpacklo_epi64(t4, t6);
            let u5 = _mm256_unpackhi_epi64(t4, t6);
            let u6 = _mm256_unpacklo_epi64(t5, t7);
            let u7 = _mm256_unpackhi_epi64(t5, t7);
            io[0] = _mm256_permute2x128_si256::<0x20>(u0, u4);
            io[1] = _mm256_permute2x128_si256::<0x20>(u1, u5);
            io[2] = _mm256_permute2x128_si256::<0x20>(u2, u6);
            io[3] = _mm256_permute2x128_si256::<0x20>(u3, u7);
            io[4] = _mm256_permute2x128_si256::<0x31>(u0, u4);
            io[5] = _mm256_permute2x128_si256::<0x31>(u1, u5);
            io[6] = _mm256_permute2x128_si256::<0x31>(u2, u6);
            io[7] = _mm256_permute2x128_si256::<0x31>(u3, u7);
        }
    }
}

/// AVX2 (8-lane) batch.
///
/// # Safety
/// AVX2 must be available, and `bufs`/`lens` must satisfy the contract on
/// [`compress_batch`] (`len() == 8`, each buffer zero-padded to its blocks).
#[target_feature(enable = "avx2")]
pub unsafe fn mix_in_avx2(bufs: &[&[u32]], lens: &[usize], acc: &mut LtHash) {
    unsafe { compress_batch::<__m256i>(bufs, lens, acc) }
}

// ---------------------------- AVX512 backend ----------------------------

impl Lanes for __m512i {
    const N: usize = 16;
    type Mask = __mmask16;

    #[inline(always)]
    unsafe fn splat(x: u32) -> Self {
        unsafe { _mm512_set1_epi32(x as i32) }
    }
    #[inline(always)]
    unsafe fn load(p: *const u32) -> Self {
        unsafe { _mm512_loadu_si512(p as *const _) }
    }
    #[inline(always)]
    unsafe fn store(self, p: *mut u32) {
        unsafe { _mm512_storeu_si512(p as *mut _, self) }
    }
    #[inline(always)]
    unsafe fn add(self, o: Self) -> Self {
        unsafe { _mm512_add_epi32(self, o) }
    }
    #[inline(always)]
    unsafe fn xor(self, o: Self) -> Self {
        unsafe { _mm512_xor_si512(self, o) }
    }
    #[inline(always)]
    unsafe fn and(self, o: Self) -> Self {
        unsafe { _mm512_and_si512(self, o) }
    }
    #[inline(always)]
    unsafe fn ror<const R: u32>(self) -> Self {
        // Native rotate (vprold), as firedancer's `wwu_ror` (`fd_avx512_wwu.h`).
        // `_mm512_ror_epi32` takes an i32 const, so match the const `R` to a
        // literal arm.
        unsafe {
            match R {
                16 => _mm512_ror_epi32::<16>(self),
                12 => _mm512_ror_epi32::<12>(self),
                8 => _mm512_ror_epi32::<8>(self),
                7 => _mm512_ror_epi32::<7>(self),
                _ => unreachable!(),
            }
        }
    }
    #[inline(always)]
    unsafe fn mask(active: &[bool]) -> Self::Mask {
        let mut m: u16 = 0;
        for (i, &a) in active.iter().enumerate() {
            m |= (a as u16) << i;
        }
        m
    }
    #[inline(always)]
    unsafe fn select(m: Self::Mask, if_active: Self, if_inactive: Self) -> Self {
        // mask_blend(k, a, b) -> b where k set, else a.
        unsafe { _mm512_mask_blend_epi32(m, if_inactive, if_active) }
    }
    #[inline(always)]
    unsafe fn transpose(io: &mut [Self]) {
        // Transliteration of firedancer's `wwu_transpose_16x16`
        // (`src/util/simd/fd_avx512_wwu.h`): two shuffle_i32x4 stages (4x4-block
        // transpose) then two unpack_epi32 stages (inner 4x4 transpose).
        // `io.len() == 16`.
        unsafe {
            let r = |i: usize| io[i];
            // Outer 4x4 transpose of 4x4 blocks (stage A).
            let t0 = _mm512_shuffle_i32x4::<0x88>(r(0), r(4));
            let t1 = _mm512_shuffle_i32x4::<0x88>(r(1), r(5));
            let t2 = _mm512_shuffle_i32x4::<0x88>(r(2), r(6));
            let t3 = _mm512_shuffle_i32x4::<0x88>(r(3), r(7));
            let t4 = _mm512_shuffle_i32x4::<0xdd>(r(0), r(4));
            let t5 = _mm512_shuffle_i32x4::<0xdd>(r(1), r(5));
            let t6 = _mm512_shuffle_i32x4::<0xdd>(r(2), r(6));
            let t7 = _mm512_shuffle_i32x4::<0xdd>(r(3), r(7));
            let t8 = _mm512_shuffle_i32x4::<0x88>(r(8), r(12));
            let t9 = _mm512_shuffle_i32x4::<0x88>(r(9), r(13));
            let ta = _mm512_shuffle_i32x4::<0x88>(r(10), r(14));
            let tb = _mm512_shuffle_i32x4::<0x88>(r(11), r(15));
            let tc = _mm512_shuffle_i32x4::<0xdd>(r(8), r(12));
            let td = _mm512_shuffle_i32x4::<0xdd>(r(9), r(13));
            let te = _mm512_shuffle_i32x4::<0xdd>(r(10), r(14));
            let tf = _mm512_shuffle_i32x4::<0xdd>(r(11), r(15));
            // Stage B.
            let r0 = _mm512_shuffle_i32x4::<0x88>(t0, t8);
            let r1 = _mm512_shuffle_i32x4::<0x88>(t1, t9);
            let r2 = _mm512_shuffle_i32x4::<0x88>(t2, ta);
            let r3 = _mm512_shuffle_i32x4::<0x88>(t3, tb);
            let r4 = _mm512_shuffle_i32x4::<0x88>(t4, tc);
            let r5 = _mm512_shuffle_i32x4::<0x88>(t5, td);
            let r6 = _mm512_shuffle_i32x4::<0x88>(t6, te);
            let r7 = _mm512_shuffle_i32x4::<0x88>(t7, tf);
            let r8 = _mm512_shuffle_i32x4::<0xdd>(t0, t8);
            let r9 = _mm512_shuffle_i32x4::<0xdd>(t1, t9);
            let ra = _mm512_shuffle_i32x4::<0xdd>(t2, ta);
            let rb = _mm512_shuffle_i32x4::<0xdd>(t3, tb);
            let rc = _mm512_shuffle_i32x4::<0xdd>(t4, tc);
            let rd = _mm512_shuffle_i32x4::<0xdd>(t5, td);
            let re = _mm512_shuffle_i32x4::<0xdd>(t6, te);
            let rf = _mm512_shuffle_i32x4::<0xdd>(t7, tf);
            // Inner 4x4 transpose of 1x1 blocks (stage C).
            let t0 = _mm512_unpacklo_epi32(r0, r2);
            let t1 = _mm512_unpacklo_epi32(r1, r3);
            let t2 = _mm512_unpackhi_epi32(r0, r2);
            let t3 = _mm512_unpackhi_epi32(r1, r3);
            let t4 = _mm512_unpacklo_epi32(r4, r6);
            let t5 = _mm512_unpacklo_epi32(r5, r7);
            let t6 = _mm512_unpackhi_epi32(r4, r6);
            let t7 = _mm512_unpackhi_epi32(r5, r7);
            let t8 = _mm512_unpacklo_epi32(r8, ra);
            let t9 = _mm512_unpacklo_epi32(r9, rb);
            let ta = _mm512_unpackhi_epi32(r8, ra);
            let tb = _mm512_unpackhi_epi32(r9, rb);
            let tc = _mm512_unpacklo_epi32(rc, re);
            let td = _mm512_unpacklo_epi32(rd, rf);
            let te = _mm512_unpackhi_epi32(rc, re);
            let tf = _mm512_unpackhi_epi32(rd, rf);
            // Stage D.
            io[0] = _mm512_unpacklo_epi32(t0, t1);
            io[1] = _mm512_unpackhi_epi32(t0, t1);
            io[2] = _mm512_unpacklo_epi32(t2, t3);
            io[3] = _mm512_unpackhi_epi32(t2, t3);
            io[4] = _mm512_unpacklo_epi32(t4, t5);
            io[5] = _mm512_unpackhi_epi32(t4, t5);
            io[6] = _mm512_unpacklo_epi32(t6, t7);
            io[7] = _mm512_unpackhi_epi32(t6, t7);
            io[8] = _mm512_unpacklo_epi32(t8, t9);
            io[9] = _mm512_unpackhi_epi32(t8, t9);
            io[10] = _mm512_unpacklo_epi32(ta, tb);
            io[11] = _mm512_unpackhi_epi32(ta, tb);
            io[12] = _mm512_unpacklo_epi32(tc, td);
            io[13] = _mm512_unpackhi_epi32(tc, td);
            io[14] = _mm512_unpacklo_epi32(te, tf);
            io[15] = _mm512_unpackhi_epi32(te, tf);
        }
    }
}

/// AVX512 (16-lane) batch.
///
/// # Safety
/// AVX512F must be available, and `bufs`/`lens` must satisfy the contract on
/// [`compress_batch`] (`len() == 16`, each buffer zero-padded to its blocks).
#[target_feature(enable = "avx512f")]
pub unsafe fn mix_in_avx512(bufs: &[&[u32]], lens: &[usize], acc: &mut LtHash) {
    unsafe { compress_batch::<__m512i>(bufs, lens, acc) }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            batch::{
                BatchMixInFn, CHUNK_LEN, NUM_CHUNK_WORDS,
                test_util::{expected_lthash, make_messages, random_messages, seeded_rng},
            },
            lt_hash::LtHash,
        },
    };

    /// Zero-padded `u32` buffers + real lengths, as `Accumulator::add` builds them.
    fn pad(msgs: &[&[u8]]) -> (Vec<Vec<u32>>, Vec<usize>) {
        let bufs = msgs
            .iter()
            .map(|m| {
                let mut buf = vec![0u32; NUM_CHUNK_WORDS];
                let bytes: &mut [u8] = bytemuck::cast_slice_mut(&mut buf);
                bytes[..m.len()].copy_from_slice(m);
                buf
            })
            .collect();
        let lens = msgs.iter().map(|m| m.len()).collect();
        (bufs, lens)
    }

    /// Run `mix` over `msgs` (one lane each) and check it equals the oracle both
    /// from `identity` and mixed on top of a non-identity accumulator — the kernel
    /// must group-add onto whatever is already in `acc`, not overwrite it.
    ///
    /// # Safety
    /// `mix`'s target feature must be available.
    unsafe fn check(mix: BatchMixInFn, msgs: &[Vec<u8>], ctx: &str) {
        let refs: Vec<&[u8]> = msgs.iter().map(|m| m.as_slice()).collect();
        let (owned, lens) = pad(&refs);
        let bufs: Vec<&[u32]> = owned.iter().map(|b| b.as_slice()).collect();
        let oracle = expected_lthash(&refs);

        let mut from_identity = LtHash::identity();
        unsafe { mix(&bufs, &lens, &mut from_identity) };
        assert_eq!(from_identity, oracle, "from identity [{ctx}]");

        let base = expected_lthash(&[b"preset accumulator contents".as_slice()]);
        let mut onto = base; // LtHash is `Copy` under cfg(test)
        unsafe { mix(&bufs, &lens, &mut onto) };
        let mut want = base;
        want.mix_in(&oracle);
        assert_eq!(onto, want, "onto non-identity [{ctx}]");
    }

    /// Exercise one backend: deterministic assorted lengths, the all-empty and
    /// all-full-chunk extremes, and many random full batches — all bit-exact
    /// against the oracle.
    ///
    /// # Safety
    /// `mix`'s target feature must be available; `n` is its lane count.
    unsafe fn exercise(mix: BatchMixInFn, n: usize) {
        unsafe {
            // Assorted single-chunk lengths (block boundaries, full chunk).
            check(mix, &make_messages(n), "assorted lengths");

            // Every lane empty (each is one zeroed root block = blake3("")).
            check(mix, &vec![Vec::new(); n], "all empty");

            // Every lane a full chunk (the maximum per-lane input).
            let full: Vec<Vec<u8>> = (0..n)
                .map(|j| (0..CHUNK_LEN).map(|i| (i + j) as u8).collect())
                .collect();
            check(mix, &full, "all full chunk");

            // Many random full batches (lanes of differing length exercise the
            // per-lane flags and active-lane masking).
            let seed = rand::random::<u64>();
            let mut rng = seeded_rng(seed);
            for iter in 0..128 {
                let msgs = random_messages(&mut rng, n, CHUNK_LEN);
                check(mix, &msgs, &format!("seed={seed:#018x} iter={iter}"));
            }
        }
    }

    #[test]
    fn avx2_matches_oracle() {
        if !is_x86_feature_detected!("avx2") {
            eprintln!("skipping: no AVX2");
            return;
        }
        // SAFETY: AVX2 just confirmed available.
        unsafe { exercise(mix_in_avx2, 8) };
    }

    #[test]
    fn avx512_matches_oracle() {
        if !is_x86_feature_detected!("avx512f") {
            eprintln!("skipping: no AVX512F");
            return;
        }
        // SAFETY: AVX512F just confirmed available.
        unsafe { exercise(mix_in_avx512, 16) };
    }
}
