//! Batched BLAKE3 XOF + LtHash group-add kernel.
//!
//! Background: agave's accounts lattice hash computes, for every account, a
//! 2048-byte BLAKE3 XOF and folds it (16-bit wrapping group-add) into a running
//! [`crate::lt_hash::LtHash`].  Done one account at a time (via [`LtHash::with`],
//! which finalizes blake3's XOF), the single-chunk input compression of a small
//! account wastes all but one SIMD lane, and every account round-trips a
//! 2048-byte buffer through memory plus a scalar `mix_in` loop.
//!
//! Firedancer's `fd_blake3_lthash_batch{8,16}` instead hash N independent
//! messages (one per SIMD lane, each <= 1 chunk), expand each to 2048 bytes, and
//! fuse the group-add — all in registers.  This module ports that idea to Rust.
//!
//! Layout:
//!  - `arch`: stable `core::arch` SIMD backends (AVX2 = 8 lanes, AVX512 = 16),
//!  - here: runtime dispatch, the non-SIMD `blake3`-crate fallback, and the
//!    public [`Accumulator`] API.

#[cfg(target_arch = "x86_64")]
mod arch;

use {
    crate::lt_hash::{LtHash, SingleLtHashUpdater},
    blake3::{BLOCK_LEN, CHUNK_LEN},
};

/// `u32` words in one chunk-sized lane buffer (`CHUNK_LEN / 4` = 256).
/// `CHUNK_LEN` (1024) is the max input that maps to a single BLAKE3 chunk — the
/// batch path; larger messages need the full Merkle tree and take the
/// `blake3`-crate fallback.
const NUM_CHUNK_WORDS: usize = CHUNK_LEN / 4;
/// Widest SIMD lane count (AVX512 width) = max messages staged per batch.
const MAX_LANES: usize = 16;

/// One lane's staging buffer: a whole BLAKE3 chunk of `u32` words, zero-padded
/// past the message. Fixed size so a batch of them is contiguous, fixed-stride
/// storage the SIMD kernel can read in place.
type LaneBuf = [u32; NUM_CHUNK_WORDS];

/// A read-only, matched view of one full-or-partial batch's staged lanes: for
/// each `l < len()`, `lanes[l]` is a zero-padded chunk buffer holding a message
/// of real byte length `lengths[l]` (`<= CHUNK_LEN`). `lanes.len() ==
/// lengths.len()`. Produced by [`Staging::batch`], consumed by the mix-in
/// functions and the `blake3` fallback in [`Accumulator::flush`].
#[derive(Clone, Copy)]
struct Batch<'a> {
    lanes: &'a [LaneBuf],
    lengths: &'a [usize],
}

impl Batch<'_> {
    fn len(&self) -> usize {
        self.lanes.len()
    }
}

/// The staged lanes of one pending batch and their metadata, kept together so the
/// invariants the SIMD kernel relies on live in one place: one fixed-size buffer
/// per lane, each committed message zero-padded through its final compression
/// block and no longer than `CHUNK_LEN`, and `count` completed lanes whose
/// `lengths` line up exactly. `count == lanes.len()` means the batch is full.
struct Staging {
    /// `num_available_lanes` contiguous, fixed-stride lane buffers.
    lanes: Box<[LaneBuf]>,
    /// Real byte length of each committed lane; only `lengths[..count]` is valid.
    lengths: [usize; MAX_LANES],
    /// Number of committed messages staged so far (the in-progress one is lane `count`).
    count: usize,
    /// Bytes written into the in-progress lane (`lanes[count]`) so far — the
    /// current message's length before `finish_current`. `0` between messages.
    cur_len: usize,
}

impl Staging {
    fn new(num_available_lanes: usize) -> Self {
        Self {
            lanes: vec![[0u32; NUM_CHUNK_WORDS]; num_available_lanes].into_boxed_slice(),
            lengths: [0; MAX_LANES],
            count: 0,
            cur_len: 0,
        }
    }

    /// The in-progress (not yet committed) lane's buffer, as bytes.
    fn current_lane(&self) -> &[u8] {
        bytemuck::cast_slice(self.lanes[self.count].as_slice())
    }

    /// The in-progress lane's buffer, as writable bytes.
    fn current_lane_mut(&mut self) -> &mut [u8] {
        bytemuck::cast_slice_mut(self.lanes[self.count].as_mut_slice())
    }

    /// Bytes staged for the in-progress message so far (`0` between messages).
    fn cur_len(&self) -> usize {
        self.cur_len
    }

    /// The in-progress message's staged bytes.
    fn staged_bytes(&self) -> &[u8] {
        &self.current_lane()[..self.cur_len]
    }

    /// Append `bytes` to the in-progress lane, advancing the cursor; returns
    /// `false` (writing nothing) if they wouldn't fit one chunk, so the caller can
    /// route the message to the `blake3` fallback instead.
    fn try_append(&mut self, bytes: &[u8]) -> bool {
        // `saturating_add` so a huge `bytes` can't wrap the bound check.
        let end = self.cur_len.saturating_add(bytes.len());
        if end > CHUNK_LEN {
            return false;
        }
        let start = self.cur_len;
        self.current_lane_mut()[start..end].copy_from_slice(bytes);
        self.cur_len = end;
        true
    }

    /// Abandon the in-progress message's staged bytes — it spilled to the `blake3`
    /// fallback — leaving the lane free for the next message.
    fn discard_current(&mut self) {
        self.cur_len = 0;
    }

    /// Zero-pad the in-progress message's final compression block, commit it, and
    /// report whether the batch is now full. `max(1)` keeps an empty message as a
    /// single zeroed root block (= blake3("")).
    fn finish_current(&mut self) -> bool {
        let len = self.cur_len;
        let valid = len.max(1).next_multiple_of(BLOCK_LEN);
        self.current_lane_mut()[len..valid].fill(0);
        self.lengths[self.count] = len;
        self.count = self.count.wrapping_add(1);
        self.cur_len = 0;
        self.count == self.lanes.len()
    }

    /// A matched view of the `count` completed lanes and their lengths.
    fn batch(&self) -> Batch<'_> {
        Batch {
            lanes: &self.lanes[..self.count],
            lengths: &self.lengths[..self.count],
        }
    }

    fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn clear(&mut self) {
        self.count = 0;
    }
}

impl core::fmt::Debug for Staging {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Omit the large lane buffers; the committed lengths are the useful state.
        let lengths = &self.lengths[..self.count];
        f.debug_struct("Staging")
            .field("count", &self.count)
            .field("lengths", &lengths)
            .field("cur_len", &self.cur_len)
            .finish_non_exhaustive()
    }
}

/// A batched [`LtHash::mix_in`]: group-adds the XOF of every message in `batch`
/// into `acc`, reading each lane in place from its zero-padded buffer.
/// Implemented by the SIMD backends in [`arch`]; the non-SIMD path hashes via the
/// `blake3` crate directly (see [`Accumulator::flush`]).
///
/// # Safety
/// Caller must ensure the backend's target feature is available and uphold the
/// buffer contract documented on `arch`'s `compress_batch`.
type BatchMixInFn = unsafe fn(batch: Batch<'_>, acc: &mut LtHash);

/// Streaming accumulator for the lattice hash of many messages.
///
/// Feed each message either whole with [`add_message`](Self::add_message), or in
/// parts with [`start_message`](Self::start_message); each is group-added
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
    /// The pending batch's staged lane buffers and their metadata.
    staging: Staging,
    /// Messages per full batch = SIMD lane count of the selected backend (1 when
    /// there is no SIMD backend). A batch flushes once this many messages are staged.
    num_available_lanes: usize,
    /// The SIMD backend used for a full batch, or `None` when the CPU has none.
    batch_mix_in_fn: Option<BatchMixInFn>,
    /// Serial blake3-crate hasher: the fallback for a message that overflows one
    /// chunk (`> CHUNK_LEN`) and for the non-SIMD / partial-tail path in `flush`.
    serial_hasher: blake3::Hasher,
    /// Set when the in-progress message overflowed one chunk and is being
    /// streamed into `serial_hasher` rather than staged for the SIMD batch;
    /// carries that state across the message's remaining parts until it is committed.
    cur_spilled: bool,
}

impl core::fmt::Debug for Accumulator {
    // Omits the opaque hasher / fn ptr; `Staging`'s own `Debug` drops its buffers.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Accumulator")
            .field("acc", &self.acc)
            .field("num_available_lanes", &self.num_available_lanes)
            .field("staging", &self.staging)
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
        Self {
            acc: LtHash::identity(),
            staging: Staging::new(num_available_lanes),
            num_available_lanes,
            batch_mix_in_fn,
            serial_hasher: blake3::Hasher::new(),
            cur_spilled: false,
        }
    }

    /// Group-add the lattice hash of one whole `msg` into the running value. To
    /// supply a message in pieces instead, use [`start_message`](Self::start_message).
    pub fn add_message(&mut self, msg: &[u8]) {
        self.stage_part(msg);
        self.commit_message();
    }

    /// Begin a message assembled from parts, borrowing the accumulator until the
    /// returned [`MessageBuilder`] is finished. Feed each piece with
    /// [`add_part`](MessageBuilder::add_part) and commit with
    /// [`finish`](MessageBuilder::finish); the exclusive borrow keeps any other
    /// accumulator operation from interleaving mid-message.
    pub fn start_message(&mut self) -> MessageBuilder<'_> {
        MessageBuilder { acc: self }
    }

    /// Append `bytes` to the message currently being staged: written straight into
    /// the SIMD lane buffer, or, once the message passes [`CHUNK_LEN`], streamed
    /// into the `blake3` fallback (`commit_message` folds in the result).
    fn stage_part(&mut self, bytes: &[u8]) {
        if self.cur_spilled {
            self.serial_hasher.update(bytes);
            return;
        }
        if !self.staging.try_append(bytes) {
            self.spill_current();
            self.serial_hasher.update(bytes);
        }
    }

    /// Commit the staged message — group-add its lattice hash into the running
    /// value and start a fresh message, flushing the SIMD batch once it fills.
    fn commit_message(&mut self) {
        if self.cur_spilled {
            self.acc.mix_in(&LtHash::with(&self.serial_hasher));
            self.cur_spilled = false;
            return;
        }
        if self.staging.finish_current() {
            self.flush();
        }
    }

    /// Abandon the in-progress message's staged bytes without folding it in, so the
    /// next message starts clean — for an unfinished [`MessageBuilder`] on drop.
    fn discard_message(&mut self) {
        self.staging.discard_current();
        self.cur_spilled = false;
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
    /// the SIMD batch. Called by [`stage_part`](Self::stage_part) on overflow.
    fn spill_current(&mut self) {
        self.serial_hasher.reset();
        self.serial_hasher.update(self.staging.staged_bytes());
        self.staging.discard_current();
        self.cur_spilled = true;
    }

    /// Run the staged messages into `acc`, then reset the stage. A full batch on
    /// a SIMD backend goes through the vector kernel; anything else — no SIMD
    /// backend, or a partial tail that doesn't fill every lane — falls back to the
    /// `blake3` crate.
    fn flush(&mut self) {
        let batch = self.staging.batch();
        if let Some(simd) = self
            .batch_mix_in_fn
            .filter(|_| batch.len() == self.num_available_lanes)
        {
            // SAFETY: a SIMD backend exists only when its target feature was
            // detected, and `filter` guarantees a full batch
            // (`batch.len() == num_available_lanes == L::N`).
            unsafe { simd(batch, &mut self.acc) };
        } else {
            for (lane, &len) in batch.lanes.iter().zip(batch.lengths) {
                let bytes: &[u8] = bytemuck::cast_slice(lane.as_slice());
                // Reset before each message: the shared hasher may be dirty from a
                // prior message, flush, or `> CHUNK_LEN` add.
                self.serial_hasher.reset();
                self.serial_hasher.update(&bytes[..len]);
                self.acc.mix_in(&LtHash::with(&self.serial_hasher));
            }
        }
        self.staging.clear();
    }

    /// Flush any staged remainder and return the accumulated lattice hash,
    /// resetting the running value to [`LtHash::identity`] so the accumulator can
    /// be reused (its SIMD staging buffers are retained). Unlike
    /// [`into_lt_hash`](Self::into_lt_hash), this does not consume `self`.
    pub fn take_lt_hash(&mut self) -> LtHash {
        debug_assert!(
            self.staging.cur_len() == 0 && !self.cur_spilled,
            "take_lt_hash() called with an unfinished message in progress",
        );
        if !self.staging.is_empty() {
            self.flush();
        }
        core::mem::replace(&mut self.acc, LtHash::identity())
    }

    /// Flush any staged remainder and return the accumulated lattice hash,
    /// consuming the accumulator. See [`take_lt_hash`](Self::take_lt_hash) to drain
    /// a reusable accumulator behind a `&mut` instead.
    pub fn into_lt_hash(mut self) -> LtHash {
        self.take_lt_hash()
    }
}

/// A message being assembled part-by-part, returned by
/// [`Accumulator::start_message`]. Holds an exclusive borrow of the accumulator,
/// so no other accumulator operation can run until this message is finished. Feed
/// pieces with [`add_part`](Self::add_part), then [`finish`](Self::finish) to
/// commit; dropping the builder without finishing discards the staged bytes.
#[must_use = "a started message does nothing until `.finish()`; dropping it discards the message"]
pub struct MessageBuilder<'a> {
    acc: &'a mut Accumulator,
}

impl MessageBuilder<'_> {
    /// Append `bytes` to the message being built.
    pub fn add_part(&mut self, bytes: &[u8]) -> &mut Self {
        self.acc.stage_part(bytes);
        self
    }

    /// Commit the message: group-add its lattice hash into the accumulator's
    /// running value.
    pub fn finish(self) {
        self.acc.commit_message();
        // Committed — skip the discarding `Drop`.
        core::mem::forget(self);
    }
}

impl SingleLtHashUpdater for MessageBuilder<'_> {
    fn write_part(&mut self, bytes: &[u8]) {
        self.add_part(bytes);
    }
}

impl Drop for MessageBuilder<'_> {
    fn drop(&mut self) {
        // Unfinished: abandon the staged bytes so the next message starts clean.
        self.acc.discard_message();
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

    /// Feeding each message in parts via [`Accumulator::start_message`] must equal
    /// the oracle — including a message that overflows one chunk partway through
    /// its parts (exercising the mid-stream spill).
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
            let mut msg = streamed.start_message();
            msg.add_part(&m[..a]);
            msg.add_part(&m[a..b]);
            msg.add_part(&m[b..]);
            msg.finish();
        }
        assert_eq!(streamed.into_lt_hash(), expected_lthash(&refs));
    }

    /// Dropping a builder without `finish` discards its staged bytes, for both the
    /// staged (`<= CHUNK_LEN`) and spilled (`> CHUNK_LEN`) in-progress states.
    #[test]
    fn builder_drop_discards_unfinished() {
        let msg = b"the message that is actually committed".as_slice();

        let mut acc = Accumulator::new();
        {
            let mut abandoned = acc.start_message();
            abandoned.add_part(b"staged but");
            abandoned.add_part(b" never finished");
        } // dropped without `finish` — nothing folded in
        {
            let mut spilled = acc.start_message();
            spilled.add_part(&vec![7u8; CHUNK_LEN + 500]); // overflows → serial spill
            spilled.add_part(b"more");
        } // dropped mid-spill — must reset `cur_spilled` too
        acc.add_message(msg);

        assert_eq!(acc.into_lt_hash(), expected_lthash(&[msg]));
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
