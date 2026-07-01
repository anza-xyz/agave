//! Batched BLAKE3 XOF + LtHash group-add kernel.
//!
//! Background: agave's accounts lattice hash computes, for every account, a
//! 2048-byte BLAKE3 XOF and folds it (16-bit wrapping group-add) into a running
//! [`crate::lt_hash::LtHash`].  Done one account at a time (the `blake3` crate's
//! `into_lt_hash_xof`), the single-chunk input compression of a small account wastes
//! all but one SIMD lane, and every account round-trips a 2048-byte buffer
//! through memory plus a scalar `mix_in` loop.
//!
//! Firedancer's `fd_blake3_lthash_batch{8,16}` instead hash N independent
//! messages (one per SIMD lane, each <= 1 chunk), expand each to 2048 bytes, and
//! fuse the group-add — all in registers.  This module ports that idea to Rust.
//!
//! Layout:
//!  - `arch`: stable `core::arch` SIMD backends (AVX2 = 8 lanes, AVX512 = 16),
//!  - here: runtime dispatch, the non-SIMD `blake3`-crate fallback, and the
//!    public [`Accumulator`]/[`accumulate`] API.

#[cfg(target_arch = "x86_64")]
mod arch;

use crate::lt_hash::LtHash;

/// Max input bytes that map to a single BLAKE3 chunk — the batch path. Larger
/// messages need the full Merkle tree and take the `blake3`-crate fallback.
const CHUNK_LEN: usize = 1024;
/// Bytes in one BLAKE3 compression block.
const BLOCK_LEN: usize = 64;
/// `u32` words in one chunk-sized lane buffer (`CHUNK_LEN / 4` = 256).
const NUM_CHUNK_WORDS: usize = CHUNK_LEN / 4;
/// Widest SIMD lane count (AVX512 width) = max messages staged per batch.
const MAX_LANES: usize = 16;

/// A batched [`LtHash::mix_in`]: group-adds the XOF of exactly `bufs.len()`
/// messages into `acc`, reading each lane in place from `bufs[l]` (a `u32`
/// buffer zero-padded to its block boundary) of real byte length `lens[l]`.
/// Implemented by the SIMD backends in [`arch`]; the non-SIMD path hashes via the
/// `blake3` crate directly (see [`Accumulator::flush`]).
///
/// # Safety
/// Caller must ensure the backend's target feature is available and uphold the
/// buffer contract documented on `arch`'s `compress_batch`.
type BatchMixInFn = unsafe fn(bufs: &[&[u32]], lens: &[usize], acc: &mut LtHash);

/// Streaming accumulator for the lattice hash of many messages.
///
/// Feed each message either whole with [`add_message`](Self::add_message), or in parts with
/// [`add_part`](Self::add_part) + [`finish_message`](Self::finish_message); each is group-added
/// (LtHash `mix_in`) into the running value. Small messages (`<= CHUNK_LEN`) are
/// copied into owned, reusable staging and run through the widest SIMD mix-in
/// function the CPU supports as soon as a full batch (`lanes` of them) accumulates; larger
/// messages are hashed immediately via the single-hash `blake3` path (they need
/// the full Merkle tree). Group-add is commutative, so this reordering does not
/// change the result.
///
/// Messages are *copied* into the accumulator's owned buffers, so the caller's
/// source buffer can be transient. Those buffers are the mix-in function's input.
pub struct Accumulator {
    /// The lattice hash being built; the mix-in functions group-add into its raw elements.
    acc: LtHash,
    /// One contiguous allocation for all `num_available_lanes` lane buffers; lane
    /// `l` occupies the sub-buffer `batch_buffer[l*NUM_CHUNK_WORDS .. (l+1)*NUM_CHUNK_WORDS]`,
    /// zero-padded past its message.
    batch_buffer: Box<[u32]>,
    /// Real byte length of each pending message.
    /// `batch_lens.len()` is the pending count; capacity is fixed at `num_available_lanes`.
    batch_lens: Vec<usize>,
    /// Messages per full batch = SIMD lane count of the selected backend (1 when
    /// there is no SIMD backend). A batch flushes once this many messages are staged.
    num_available_lanes: usize,
    /// The SIMD backend used for a full batch, or `None` when the CPU has none.
    batch_mix_in_fn: Option<BatchMixInFn>,
    /// Serial blake3-crate hasher: the fallback for a message that overflows one
    /// chunk (`> CHUNK_LEN`) and for the non-SIMD / partial-tail path in `flush`.
    serial_hasher: blake3::Hasher,
    /// Bytes written into the in-progress message's lane buffer by `add_part` so
    /// far, i.e. its length before `finish_message`. `0` when no message is in progress.
    cur_len: usize,
    /// Set when the in-progress message overflowed one chunk and is being
    /// streamed into `serial_hasher` rather than staged for the SIMD batch;
    /// carries that state across the message's remaining `add_part`s until `finish_message`.
    cur_spilled: bool,
}

impl core::fmt::Debug for Accumulator {
    // Omits the large lane `batch_buffer` scratch (and the opaque hasher / fn ptr).
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Accumulator")
            .field("acc", &self.acc)
            .field("num_available_lanes", &self.num_available_lanes)
            .field("batch_lens", &self.batch_lens)
            .field("cur_len", &self.cur_len)
            .field("cur_spilled", &self.cur_spilled)
            .finish_non_exhaustive()
    }
}

impl Accumulator {
    pub fn new() -> Self {
        /// Pick the widest lane count + mix-in function this CPU supports, falling
        /// back to a single lane (the `blake3`-crate fallback) when there is no
        /// SIMD backend.
        fn select_mix_in_fn() -> (usize, Option<BatchMixInFn>) {
            #[cfg(target_arch = "x86_64")]
            {
                if is_x86_feature_detected!("avx512f") {
                    return (16, Some(arch::mix_in_avx512));
                }
                if is_x86_feature_detected!("avx2") {
                    return (8, Some(arch::mix_in_avx2));
                }
            }
            (1, None)
        }
        let (num_available_lanes, batch_mix_in_fn) = select_mix_in_fn();
        // usize needs to be `<= 16 * 256`
        let batch_words = num_available_lanes
            .checked_mul(NUM_CHUNK_WORDS)
            .expect("batch buffer word count fits usize");
        Self {
            acc: LtHash::identity(),
            batch_buffer: vec![0u32; batch_words].into_boxed_slice(),
            batch_lens: Vec::with_capacity(num_available_lanes),
            num_available_lanes,
            batch_mix_in_fn,
            serial_hasher: blake3::Hasher::new(),
            cur_len: 0,
            cur_spilled: false,
        }
    }

    /// Append `bytes` to the message currently being built; call [`finish_message`] to
    /// finish it. Feeding a message in parts lets the caller avoid assembling a
    /// contiguous buffer — the parts are written straight into the SIMD lane
    /// buffer, or, once a message passes [`CHUNK_LEN`], streamed into the `blake3`
    /// fallback.
    ///
    /// [`finish_message`]: Self::finish_message
    pub fn add_part(&mut self, bytes: &[u8]) {
        if self.cur_spilled {
            self.serial_hasher.update(bytes);
            return;
        }
        // `saturating_add` so a huge `bytes` can't wrap the bound check; then this
        // message is simply routed to the fallback below.
        let end = self.cur_len.saturating_add(bytes.len());
        if end <= CHUNK_LEN {
            // Fits one chunk: write straight into the current lane buffer.
            let start = self.batch_lens.len().wrapping_mul(NUM_CHUNK_WORDS);
            let lane_bytes: &mut [u8] = bytemuck::cast_slice_mut(
                &mut self.batch_buffer[start..start.wrapping_add(NUM_CHUNK_WORDS)],
            );
            lane_bytes[self.cur_len..end].copy_from_slice(bytes);
            self.cur_len = end;
        } else {
            // Overflows one chunk: hand the bytes buffered so far, plus `bytes`, to
            // the serial blake3 hasher; `finish_message` folds in the result.
            self.spill_current();
            self.serial_hasher.update(bytes);
        }
    }

    /// Finish the message built by [`add_part`], fold its lattice hash into the
    /// running value, and start a fresh message.
    ///
    /// [`add_part`]: Self::add_part
    pub fn finish_message(&mut self) {
        if self.cur_spilled {
            self.acc.mix_in(&LtHash::with(&self.serial_hasher));
            self.cur_spilled = false;
            self.cur_len = 0;
            return;
        }
        // Zero-pad the message's last block, stage its length, and flush once a
        // full batch has accumulated. `max(1)` keeps an empty message as a single
        // zeroed root block (= blake3("")).
        let valid = self.cur_len.max(1).next_multiple_of(BLOCK_LEN);
        let start = self.batch_lens.len().wrapping_mul(NUM_CHUNK_WORDS);
        let lane_bytes: &mut [u8] = bytemuck::cast_slice_mut(
            &mut self.batch_buffer[start..start.wrapping_add(NUM_CHUNK_WORDS)],
        );
        lane_bytes[self.cur_len..valid].fill(0);
        self.batch_lens.push(self.cur_len);
        self.cur_len = 0;
        if self.batch_lens.len() == self.num_available_lanes {
            self.flush();
        }
    }

    /// Group-add the lattice hash of one whole `msg` into the running value:
    /// [`add_part`] then [`finish_message`].
    ///
    /// [`add_part`]: Self::add_part
    /// [`finish_message`]: Self::finish_message
    pub fn add_message(&mut self, msg: &[u8]) {
        self.add_part(msg);
        self.finish_message();
    }

    /// Group-add a precomputed `LtHash` (e.g. one produced elsewhere, or another
    /// accumulator's [`into_lt_hash`]) into the running value, without hashing any
    /// message.
    ///
    /// [`into_lt_hash`]: Self::into_lt_hash
    pub fn mix_in(&mut self, hash: &LtHash) {
        self.acc.mix_in(hash);
    }

    /// Move the bytes buffered for the current message into `serial_hasher` and
    /// mark it spilled — its message exceeds one chunk, so it can't be staged for
    /// the SIMD batch. Called by [`add_part`](Self::add_part) on overflow.
    fn spill_current(&mut self) {
        self.serial_hasher.reset();
        let start = self.batch_lens.len().wrapping_mul(NUM_CHUNK_WORDS);
        let lane_bytes: &[u8] =
            bytemuck::cast_slice(&self.batch_buffer[start..start.wrapping_add(NUM_CHUNK_WORDS)]);
        self.serial_hasher.update(&lane_bytes[..self.cur_len]);
        self.cur_spilled = true;
    }

    /// Run the staged messages into `acc`, then reset the stage. A full batch on
    /// a SIMD backend goes through the vector kernel; anything else — no SIMD
    /// backend, or a partial tail that doesn't fill every lane — falls back to the
    /// `blake3` crate.
    fn flush(&mut self) {
        let count = self.batch_lens.len();
        if let Some(simd) = self
            .batch_mix_in_fn
            .filter(|_| count == self.num_available_lanes)
        {
            let mut bufs: [&[u32]; MAX_LANES] = [&[][..]; MAX_LANES];
            for (b, lane_buf) in bufs
                .iter_mut()
                .zip(self.batch_buffer.chunks_exact(NUM_CHUNK_WORDS))
            {
                *b = lane_buf;
            }
            // SAFETY: a SIMD backend exists only when its target feature was
            // detected, and `filter` guarantees a full batch
            // (`count == num_available_lanes == L::N`).
            unsafe { simd(&bufs[..count], self.batch_lens.as_slice(), &mut self.acc) };
        } else {
            for (lane_buf, &len) in self
                .batch_buffer
                .chunks_exact(NUM_CHUNK_WORDS)
                .zip(&self.batch_lens)
            {
                let bytes: &[u8] = bytemuck::cast_slice(lane_buf);
                // Reset before each message: the shared hasher may be dirty from a
                // prior message, flush, or `> CHUNK_LEN` add.
                self.serial_hasher.reset();
                self.serial_hasher.update(&bytes[..len]);
                self.acc.mix_in(&LtHash::with(&self.serial_hasher));
            }
        }
        self.batch_lens.clear();
    }

    /// Flush any staged remainder and return the accumulated lattice hash.
    pub fn into_lt_hash(mut self) -> LtHash {
        debug_assert!(
            self.cur_len == 0 && !self.cur_spilled,
            "into_lt_hash() called with an unfinish_messageted message in progress",
        );
        if !self.batch_lens.is_empty() {
            self.flush();
        }
        self.acc
    }
}

/// Test helpers shared by this module's tests and the backend tests in `arch`.
#[cfg(test)]
pub(super) mod test_util {
    // Bounded index/offset math in test fixtures; overflow checks add no value.
    #![allow(clippy::arithmetic_side_effects)]
    use {
        super::*,
        rand::{Rng, RngCore, SeedableRng},
        rand_chacha::ChaChaRng,
    };

    /// A deterministic (seeded) RNG, so a failing randomized test reproduces.
    pub fn seeded_rng(seed: u64) -> ChaChaRng {
        ChaChaRng::seed_from_u64(seed)
    }

    /// `n` messages of uniformly random length in `0..=max_len` filled with
    /// random bytes.
    pub fn random_messages(rng: &mut ChaChaRng, n: usize, max_len: usize) -> Vec<Vec<u8>> {
        (0..n)
            .map(|_| {
                let mut m = vec![0u8; rng.random_range(0..=max_len)];
                rng.fill_bytes(&mut m);
                m
            })
            .collect()
    }

    /// Independent oracle: the lattice hash of `messages` via the `blake3` crate
    /// — `LtHash::with` + `mix_in` over the batch, the same blake3 reference the
    /// mix-in functions are checked against. Reuses one `Hasher`, resetting
    /// between messages.
    pub fn expected_lthash(messages: &[&[u8]]) -> LtHash {
        let mut acc = LtHash::identity();
        let mut hasher = blake3::Hasher::new();
        for msg in messages {
            hasher.reset();
            hasher.update(msg);
            acc.mix_in(&LtHash::with(&hasher));
        }
        acc
    }

    /// `n` deterministic messages of assorted single-chunk lengths (exercising
    /// partial/exact/multi-block and the single-block and full-chunk boundaries).
    pub fn make_messages(n: usize) -> Vec<Vec<u8>> {
        let lens = [
            0usize, 1, 5, 63, 64, 65, 127, 200, 273, 512, 700, 960, 1000, 1023, 1024, 333,
        ];
        (0..n)
            .map(|j| {
                let len = lens[j % lens.len()];
                (0..len)
                    .map(|i| (i.wrapping_mul(31).wrapping_add(j * 7)) as u8)
                    .collect()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    // Bounded index/offset math in test fixtures; overflow checks add no value.
    #![allow(clippy::arithmetic_side_effects)]
    use {
        super::{test_util::*, *},
        rand::{Rng, seq::SliceRandom},
    };

    /// Mix the lattice hash of every message in `messages` into `acc`.
    fn accumulate(messages: &[&[u8]], acc: &mut LtHash) {
        let mut a = Accumulator::new();
        for &m in messages {
            a.add_message(m);
        }
        acc.mix_in(&a.into_lt_hash());
    }

    /// Both entry points — one-shot `accumulate` and one-by-one streaming `add_message`
    /// (whatever batch size this CPU picks, its partial tail, and the
    /// `> CHUNK_LEN` fallback) — must equal the independent `blake3`-crate oracle.
    fn assert_matches_oracle(msgs: &[Vec<u8>], ctx: &str) {
        let refs: Vec<&[u8]> = msgs.iter().map(|m| m.as_slice()).collect();
        let expected = expected_lthash(&refs);

        let mut one_shot = LtHash::identity();
        accumulate(&refs, &mut one_shot);
        assert_eq!(one_shot, expected, "one-shot accumulate [{ctx}]");

        let mut streamed = Accumulator::new();
        for r in &refs {
            streamed.add_message(r);
        }
        assert_eq!(streamed.into_lt_hash(), expected, "streamed add [{ctx}]");
    }

    #[test]
    fn accumulate_matches_oracle() {
        // 35 messages: not a multiple of 8 or 16, with oversized ones that
        // exercise the multi-chunk blake3 fallback.
        let mut msgs = make_messages(35);
        msgs[3] = (0..5000u32).map(|i| i as u8).collect(); // > CHUNK_LEN
        msgs[30] = (0..1500u32).map(|i| (i * 3) as u8).collect(); // > CHUNK_LEN
        assert_matches_oracle(&msgs, "fixed 35 + oversized");
    }

    /// Counts bracketing both the 8- and 16-lane batch boundaries, plus several
    /// full batches followed by a partial tail.
    #[test]
    fn batch_size_boundaries() {
        for count in [0usize, 1, 7, 8, 9, 15, 16, 17, 31, 32, 33, 48, 100] {
            assert_matches_oracle(&make_messages(count), &format!("count={count}"));
        }
    }

    /// Empty / degenerate and boundary-length inputs.
    #[test]
    fn edge_cases() {
        assert_eq!(Accumulator::new().into_lt_hash(), LtHash::identity()); // no messages
        assert_matches_oracle(&[vec![]], "one empty"); // = blake3("")
        assert_matches_oracle(&vec![vec![]; 40], "40 empty"); // spanning batches

        // Every block boundary and both sides of the single-chunk limit (1024),
        // including multi-chunk sizes that take the blake3 fallback.
        let lens = [
            0usize, 1, 63, 64, 65, 127, 128, 129, 1023, 1024, 1025, 2047, 2048, 4096,
        ];
        let msgs: Vec<Vec<u8>> = lens
            .iter()
            .map(|&l| (0..l).map(|i| i as u8).collect())
            .collect();
        assert_matches_oracle(&msgs, "boundary lengths");
    }

    /// Feeding each message in parts via `add_part` + `finish_message` must equal the
    /// oracle — including a message that overflows one chunk partway through its
    /// parts (exercising the mid-stream spill).
    #[test]
    fn streaming_in_parts_matches_oracle() {
        let mut msgs = make_messages(20);
        msgs.push((0..3000u32).map(|i| i as u8).collect()); // > CHUNK_LEN
        let refs: Vec<&[u8]> = msgs.iter().map(|m| m.as_slice()).collect();

        let mut streamed = Accumulator::new();
        for m in &refs {
            // Split into three parts so a chunk boundary (and the spill) can fall
            // mid-message rather than only at an `add_part` that starts one.
            let a = m.len() / 3;
            let b = m.len() * 2 / 3;
            streamed.add_part(&m[..a]);
            streamed.add_part(&m[a..b]);
            streamed.add_part(&m[b..]);
            streamed.finish_message();
        }
        assert_eq!(streamed.into_lt_hash(), expected_lthash(&refs));
    }

    /// `mix_in` of a precomputed hash composes with added messages.
    #[test]
    fn mix_in_precomputed_hash() {
        let msgs = make_messages(10);
        let refs: Vec<&[u8]> = msgs.iter().map(|m| m.as_slice()).collect();
        let (head, tail) = refs.split_at(5);

        let mut acc = Accumulator::new();
        acc.mix_in(&expected_lthash(head)); // precomputed elsewhere
        for m in tail {
            acc.add_message(m);
        }
        assert_eq!(acc.into_lt_hash(), expected_lthash(&refs));
    }

    /// Many random batches — varied counts and lengths straddling the chunk limit
    /// so both the SIMD batch path and the blake3 fallback run — bit-exact vs the
    /// oracle.
    #[test]
    fn randomized_matches_oracle() {
        let seed = rand::random::<u64>();
        let mut rng = seeded_rng(seed);
        for iter in 0..128 {
            let n = rng.random_range(0..=40);
            let msgs = random_messages(&mut rng, n, 1200);
            assert_matches_oracle(&msgs, &format!("seed={seed:#018x} iter={iter}"));
        }
    }

    /// Group-add is commutative, so the result must not depend on the order
    /// messages are added, nor on how they align to batch boundaries.
    #[test]
    fn order_independent() {
        let seed = rand::random::<u64>();
        let mut rng = seeded_rng(seed);
        let msgs = random_messages(&mut rng, 50, 1500);
        let mut refs: Vec<&[u8]> = msgs.iter().map(|m| m.as_slice()).collect();

        let mut base = LtHash::identity();
        accumulate(&refs, &mut base);
        for _ in 0..20 {
            refs.shuffle(&mut rng);
            let mut acc = LtHash::identity();
            accumulate(&refs, &mut acc);
            assert_eq!(acc, base, "order independence (seed={seed:#018x})");
        }
    }

    /// Cross-check the `Accumulator` against the canonical `hello` LtHash vector
    /// that pins down `LtHash::with` in `lt_hash.rs`.
    #[test]
    fn known_vector() {
        let hello_head: [u16; 8] = [
            0x8fea, 0x3d16, 0x86b3, 0x9282, 0x445e, 0xc591, 0x8de5, 0xb34b,
        ];
        let mut a = Accumulator::new();
        a.add_message(b"hello");
        assert_eq!(&a.into_lt_hash().0[..8], &hello_head);
    }
}
