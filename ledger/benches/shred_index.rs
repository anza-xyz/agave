#![feature(test)]
extern crate test;

use {
    bitvec::{array::BitArray, order::Lsb0},
    rand::Rng,
    serde::{Deserialize, Serialize},
    serde_with::serde_as,
    solana_ledger::blockstore_meta::ShredIndex as LegacyShredIndex,
    std::ops::{Bound, RangeBounds},
    test::Bencher,
};

const MAX_DATA_SHREDS_PER_SLOT: usize = 32_768;
const NUM_BYTES: usize = (MAX_DATA_SHREDS_PER_SLOT + 7) / 8;
const NUM_U64S: usize = (MAX_DATA_SHREDS_PER_SLOT + 63) / 64;

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct ByteShredIndex {
    #[serde_as(as = "[_; NUM_BYTES]")]
    index: [u8; NUM_BYTES],
    num_shreds: usize,
}

impl Default for ByteShredIndex {
    fn default() -> Self {
        Self {
            index: [0; NUM_BYTES],
            num_shreds: 0,
        }
    }
}

impl ByteShredIndex {
    fn new() -> Self {
        Self::default()
    }

    pub fn contains(&self, idx: u64) -> bool {
        if idx >= MAX_DATA_SHREDS_PER_SLOT as u64 {
            return false;
        }
        let byte_idx = (idx / 8) as usize;
        let bit_idx = (idx % 8) as u8;
        (self.index[byte_idx] & (1 << bit_idx)) != 0
    }

    pub fn insert(&mut self, idx: u64) {
        if idx >= MAX_DATA_SHREDS_PER_SLOT as u64 {
            return;
        }
        let byte_idx = (idx / 8) as usize;
        let bit_idx = (idx % 8) as u8;
        let old_byte = self.index[byte_idx];
        let new_byte = old_byte | (1 << bit_idx);
        if old_byte != new_byte {
            self.index[byte_idx] = new_byte;
            self.num_shreds += 1;
        }
    }

    pub fn range<R>(&self, bounds: R) -> impl Iterator<Item = u64> + '_
    where
        R: RangeBounds<u64>,
    {
        let start = match bounds.start_bound() {
            Bound::Included(&n) => n,
            Bound::Excluded(&n) => n + 1,
            Bound::Unbounded => 0,
        };
        let end = match bounds.end_bound() {
            Bound::Included(&n) => n + 1,
            Bound::Excluded(&n) => n,
            Bound::Unbounded => MAX_DATA_SHREDS_PER_SLOT as u64,
        };

        let start_byte = (start / 8) as usize;
        let end_byte = ((end + 7) / 8) as usize;

        self.index[start_byte..end_byte]
            .iter()
            .enumerate()
            .flat_map(move |(byte_offset, &byte)| {
                let base_idx = ((start_byte + byte_offset) * 8) as u64;

                // Determine the range of bits to check in this byte
                let lower_bound = if base_idx < start {
                    (start - base_idx) as u8
                } else {
                    0
                };

                let upper_bound = if base_idx + 8 > end {
                    (end - base_idx) as u8
                } else {
                    8
                };

                // Create a mask for the relevant bits in this byte
                let mask = if lower_bound == 0 && upper_bound == 8 {
                    byte
                } else {
                    let lower_mask = !0u8 << lower_bound;
                    let upper_mask = !0u8 >> (8 - upper_bound);
                    byte & lower_mask & upper_mask
                };

                // Iterate through the bits set in the mask
                std::iter::from_fn({
                    let mut remaining = mask;
                    move || {
                        if remaining == 0 {
                            None
                        } else {
                            let bit_idx = remaining.trailing_zeros() as u8;
                            remaining &= remaining - 1;
                            Some(base_idx + bit_idx as u64)
                        }
                    }
                })
            })
    }
}

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct U64ShredIndex {
    #[serde_as(as = "[_; NUM_U64S]")]
    index: [u64; NUM_U64S],
    num_shreds: usize,
}

impl Default for U64ShredIndex {
    fn default() -> Self {
        Self {
            index: [0; NUM_U64S],
            num_shreds: 0,
        }
    }
}

impl U64ShredIndex {
    fn new() -> Self {
        Self::default()
    }

    pub fn contains(&self, idx: u64) -> bool {
        if idx >= MAX_DATA_SHREDS_PER_SLOT as u64 {
            return false;
        }
        let word_idx = idx / 64;
        let bit_idx = idx % 64;
        (self.index[word_idx as usize] & (1 << bit_idx)) != 0
    }

    pub fn insert(&mut self, idx: u64) {
        if idx >= MAX_DATA_SHREDS_PER_SLOT as u64 {
            return;
        }
        let word_idx = idx / 64;
        let bit_idx = idx % 64;
        let old_word = self.index[word_idx as usize];
        let new_word = old_word | (1 << bit_idx);
        if old_word != new_word {
            self.index[word_idx as usize] = new_word;
            self.num_shreds += 1;
        }
    }

    /// Provides an iterator over the set shred indices within a specified range.
    ///
    /// # Algorithm
    /// 1. Divide the specified range into 64-bit words.
    /// 2. For each word:
    ///    - Calculate the base index (position of the word * 64).
    ///    - Process all set bits in the word.
    ///    - For words overlapping the range boundaries:
    ///      - Determine the relevant bit range using boundaries.
    ///      - Mask out bits outside the range.
    ///    - Use bit manipulation to iterate over set bits efficiently.
    ///
    /// ## Explanation
    /// > Note we're showing 32 bits per word in examples for brevity, but each word is 64 bits.
    ///
    /// Given range `[75..205]`:
    ///
    /// Word layout (each word is 64 bits), where each X represents a bit candidate:
    /// ```text
    /// Word 1 (0–63):    [................................] ← Not included (outside range)
    /// Word 2 (64–127):  [..........XXXXXXXXXXXXXXXXXXXXXX] ← Partial word (start)
    /// Word 3 (128–191): [XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX] ← Full word (entirely in range)
    /// Word 4 (192–255): [XXXXXXXXXXXXXXXXXXX.............] ← Partial word (end)
    /// ```
    ///
    /// Partial Word 2 (contains start boundary 75):
    /// - Base index = 64
    /// - Lower boundary = 75 - 64 = 11
    /// - Lower mask = `11111111111111110000000000000000`
    ///
    /// Partial Word 4 (contains end boundary 205):
    /// - Base index = 192
    /// - Upper boundary = 205 - 192 = 13
    /// - Upper mask = `00000000000000000000000000001111`
    ///
    /// Final mask = `word & lower_mask & upper_mask`
    ///
    /// Bit iteration:
    /// 1. Apply masks to restrict the bits to the range.
    /// 2. While bits remain in the masked word:
    ///    a. Find the lowest set bit (`trailing_zeros`).
    ///    b. Add the bit's position to the base index.
    ///    c. Clear the lowest set bit (`n & (n - 1)`).
    /// ```
    pub fn range<R>(&self, bounds: R) -> impl Iterator<Item = u64> + '_
    where
        R: RangeBounds<u64>,
    {
        let start = match bounds.start_bound() {
            Bound::Included(&n) => n,
            Bound::Excluded(&n) => n + 1,
            Bound::Unbounded => 0,
        };
        let end = match bounds.end_bound() {
            Bound::Included(&n) => n + 1,
            Bound::Excluded(&n) => n,
            Bound::Unbounded => MAX_DATA_SHREDS_PER_SLOT as u64,
        };

        let start_word = (start / 64) as usize;
        let end_word: usize = ((end + 63) / 64) as usize;

        self.index[start_word..end_word]
            .iter()
            .enumerate()
            .flat_map(move |(word_offset, &word)| {
                let base_idx = (start_word + word_offset) as u64 * 64;

                // Determine the range of bits to check in this word
                let lower_bound = if base_idx < start {
                    start - base_idx
                } else {
                    0
                };

                let upper_bound = if base_idx + 64 > end {
                    end - base_idx
                } else {
                    64
                };

                // Create a mask for the relevant bits in this word
                let mask = if lower_bound == 0 && upper_bound == 64 {
                    word
                } else {
                    let lower_mask = !0u64 << lower_bound;
                    let upper_mask = !0u64 >> (64 - upper_bound);
                    word & lower_mask & upper_mask
                };

                std::iter::from_fn({
                    let mut remaining = mask;
                    move || {
                        if remaining == 0 {
                            None
                        } else {
                            let bit_idx = remaining.trailing_zeros() as u64;
                            // Clear the lowest set bit
                            remaining &= remaining - 1;
                            Some(base_idx + bit_idx)
                        }
                    }
                })
            })
    }
}

type ShredBitArray = BitArray<[u64; NUM_U64S], Lsb0>;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
struct BitArrayShredIndex {
    index: ShredBitArray,
    num_shreds: usize,
}

impl BitArrayShredIndex {
    pub fn new() -> Self {
        Self {
            index: BitArray::ZERO,
            num_shreds: 0,
        }
    }

    pub fn contains(&self, idx: u64) -> bool {
        if idx >= MAX_DATA_SHREDS_PER_SLOT as u64 {
            return false;
        }
        self.index[idx as usize]
    }

    pub fn insert(&mut self, idx: u64) {
        if idx >= MAX_DATA_SHREDS_PER_SLOT as u64 {
            return;
        }
        if !self.index[idx as usize] {
            self.index.set(idx as usize, true);
            self.num_shreds += 1;
        }
    }

    pub fn range<R>(&self, bounds: R) -> impl Iterator<Item = u64> + '_
    where
        R: RangeBounds<u64>,
    {
        let start = match bounds.start_bound() {
            Bound::Included(&n) => n,
            Bound::Excluded(&n) => n + 1,
            Bound::Unbounded => 0,
        };
        let end = match bounds.end_bound() {
            Bound::Included(&n) => n + 1,
            Bound::Excluded(&n) => n,
            Bound::Unbounded => MAX_DATA_SHREDS_PER_SLOT as u64,
        };

        (start..end).filter(|&idx| self.index[idx as usize])
    }
}

use bv::BitVec;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct BvShredIndex {
    index: BitVec<u8>,
    num_shreds: usize,
}

impl Default for BvShredIndex {
    fn default() -> Self {
        Self {
            index: BitVec::new_fill(false, MAX_DATA_SHREDS_PER_SLOT as u64),
            num_shreds: 0,
        }
    }
}

impl BvShredIndex {
    fn new() -> Self {
        Self::default()
    }

    pub fn contains(&self, idx: u64) -> bool {
        if idx >= MAX_DATA_SHREDS_PER_SLOT as u64 {
            return false;
        }
        self.index.get(idx)
    }

    pub fn insert(&mut self, idx: u64) {
        if idx >= MAX_DATA_SHREDS_PER_SLOT as u64 {
            return;
        }
        if !self.index.get(idx) {
            self.index.set(idx, true);
            self.num_shreds += 1;
        }
    }

    pub fn range<R>(&self, bounds: R) -> impl Iterator<Item = u64> + '_
    where
        R: RangeBounds<u64>,
    {
        let start = match bounds.start_bound() {
            Bound::Included(&n) => n,
            Bound::Excluded(&n) => n + 1,
            Bound::Unbounded => 0,
        };
        let end = match bounds.end_bound() {
            Bound::Included(&n) => n + 1,
            Bound::Excluded(&n) => n,
            Bound::Unbounded => MAX_DATA_SHREDS_PER_SLOT as u64,
        };

        (start..end).filter(|&i| self.index.get(i))
    }
}

/// Benchmarks range operations with different window sizes on sparse data.
///
/// This test:
/// - Creates a new empty index
/// - Randomly sets ~10% of bits across the entire possible range
/// - Tests iteration over windows of different sizes (64, 1024, 4096 bits)
/// - Measures the performance impact of window size on range operations
macro_rules! bench_range_window {
    ($name:ident, $index:ty, $window_size:expr) => {
        #[bench]
        fn $name(b: &mut Bencher) {
            let mut index = <$index>::new();
            let mut rng = rand::thread_rng();

            // Set ~10% of bits randomly
            for _ in 0..(MAX_DATA_SHREDS_PER_SLOT as f64 * 0.1) as usize {
                index.insert(rng.gen_range(0..MAX_DATA_SHREDS_PER_SLOT as u64));
            }

            let start = rng.gen_range(0..MAX_DATA_SHREDS_PER_SLOT as u64 - $window_size);
            b.iter(|| {
                let count = index.range(start..(start + $window_size)).count();
                test::black_box(count)
            });
        }
    };
}

/// This test:
/// - Creates a new empty index
/// - Populates it with specified density (10%, 50%, 90%)
/// - Tests performance impact of data density on range operations
/// - Allows configurable range size
macro_rules! bench_range_density {
    ($name:ident, $index:ty, $density:expr, $range_size:expr) => {
        #[bench]
        fn $name(b: &mut Bencher) {
            use rand::seq::SliceRandom;
            let mut index = <$index>::new();
            let mut rng = rand::thread_rng();

            // Calculate number of entries needed for desired density
            let range_size = $range_size;
            let num_entries = (range_size as f64 * $density) as u64;

            // Insert entries to achieve target density
            let start = 0;
            let end = start + range_size;
            let mut entries: Vec<u64> = (start..end).collect();
            entries.shuffle(&mut rng);
            entries.truncate(num_entries as usize);

            for entry in entries {
                index.insert(entry);
            }

            b.iter(|| {
                let count = index.range(start..end).count();
                test::black_box(count)
            });
        }
    };
}

/// Benchmarks range operations with different data distributions.
///
/// This test:
/// - Creates a new empty index with 50% density in 1000..2000 range
/// - Tests three patterns:
///   - Sequential (every other index)
///   - Random (randomly distributed)
///   - Clustered (groups of consecutive indices)
/// - Measures how data distribution affects range operation performance
macro_rules! bench_range_distribution {
    ($name:ident, $index:ty, $pattern:expr) => {
        #[bench]
        fn $name(b: &mut Bencher) {
            use rand::seq::SliceRandom;
            let mut index = <$index>::new();
            let mut rng = rand::thread_rng();

            match $pattern {
                "sequential" => {
                    for i in (1000..2000).step_by(2) {
                        index.insert(i);
                    }
                }
                "random" => {
                    let mut indices: Vec<u64> = (1000..2000).collect();
                    indices.shuffle(&mut rng);
                    indices.truncate(500);
                    for i in indices {
                        index.insert(i);
                    }
                }
                "clustered" => {
                    let mut current = 1000u64;
                    while current < 2000 {
                        // Insert clusters of 8 consecutive indices
                        for i in current..current.saturating_add(8).min(2000) {
                            index.insert(i);
                        }
                        // Skip 8 indices before next cluster
                        current += 16;
                    }
                }
                _ => panic!("Unknown pattern"),
            }

            b.iter(|| {
                let count = index.range(1000..2000).count();
                test::black_box(count)
            });
        }
    };
}

/// Benchmarks insertion operations with different patterns.
///
/// This test:
/// - Creates a new empty index
/// - Tests three insertion patterns:
///   - Sequential (consecutive indices)
///   - Random (randomly distributed)
///   - Clustered (groups of consecutive indices)
/// - Measures how insertion pattern affects performance
macro_rules! bench_insert_pattern {
    ($name:ident, $index:ty, $pattern:expr) => {
        #[bench]
        fn $name(b: &mut Bencher) {
            let mut index = <$index>::new();
            let mut rng = rand::thread_rng();
            let mut next_insert = 0u64;
            let mut cluster_start = 0u64;

            b.iter(|| {
                match $pattern {
                    "sequential" => {
                        index.insert(next_insert);
                        next_insert = (next_insert + 1) % MAX_DATA_SHREDS_PER_SLOT as u64;
                    }
                    "random" => {
                        index.insert(rng.gen_range(0..MAX_DATA_SHREDS_PER_SLOT as u64));
                    }
                    "clustered" => {
                        index.insert(cluster_start + (next_insert % 8));
                        next_insert += 1;
                        if next_insert % 8 == 0 {
                            cluster_start = (cluster_start + 16) % MAX_DATA_SHREDS_PER_SLOT as u64;
                        }
                    }
                    _ => panic!("Unknown pattern"),
                }
                test::black_box(&index);
            });
        }
    };
}

// Window size benchmarks (64, 1024, 4096 bits) for each implementation
bench_range_window!(bench_window_64_byte, ByteShredIndex, 64);
bench_range_window!(bench_window_1024_byte, ByteShredIndex, 1024);
bench_range_window!(bench_window_4096_byte, ByteShredIndex, 4096);

bench_range_window!(bench_window_64_bit_array, BitArrayShredIndex, 64);
bench_range_window!(bench_window_1024_bit_array, BitArrayShredIndex, 1024);
bench_range_window!(bench_window_4096_bit_array, BitArrayShredIndex, 4096);

bench_range_window!(bench_window_64_legacy, LegacyShredIndex, 64);
bench_range_window!(bench_window_1024_legacy, LegacyShredIndex, 1024);
bench_range_window!(bench_window_4096_legacy, LegacyShredIndex, 4096);

bench_range_window!(bench_window_64_bv, BvShredIndex, 64);
bench_range_window!(bench_window_1024_bv, BvShredIndex, 1024);
bench_range_window!(bench_window_4096_bv, BvShredIndex, 4096);

bench_range_window!(bench_window_64_u64, U64ShredIndex, 64);
bench_range_window!(bench_window_1024_u64, U64ShredIndex, 1024);
bench_range_window!(bench_window_4096_u64, U64ShredIndex, 4096);

// Density benchmarks (10%, 50%, 90%) for each implementation
bench_range_density!(bench_density_10_byte, ByteShredIndex, 0.1, 1000);
bench_range_density!(bench_density_50_byte, ByteShredIndex, 0.5, 1000);
bench_range_density!(bench_density_90_byte, ByteShredIndex, 0.9, 1000);
bench_range_density!(bench_density_10_byte_10k, ByteShredIndex, 0.1, 10000);
bench_range_density!(bench_density_50_byte_10k, ByteShredIndex, 0.5, 10000);
bench_range_density!(bench_density_90_byte_10k, ByteShredIndex, 0.9, 10000);

bench_range_density!(bench_density_10_bit_array, BitArrayShredIndex, 0.1, 1000);
bench_range_density!(bench_density_50_bit_array, BitArrayShredIndex, 0.5, 1000);
bench_range_density!(bench_density_90_bit_array, BitArrayShredIndex, 0.9, 1000);
bench_range_density!(
    bench_density_10_bit_array_10k,
    BitArrayShredIndex,
    0.1,
    10000
);
bench_range_density!(
    bench_density_50_bit_array_10k,
    BitArrayShredIndex,
    0.5,
    10000
);
bench_range_density!(
    bench_density_90_bit_array_10k,
    BitArrayShredIndex,
    0.9,
    10000
);

bench_range_density!(bench_density_10_legacy, LegacyShredIndex, 0.1, 1000);
bench_range_density!(bench_density_50_legacy, LegacyShredIndex, 0.5, 1000);
bench_range_density!(bench_density_90_legacy, LegacyShredIndex, 0.9, 1000);
bench_range_density!(bench_density_10_legacy_10k, LegacyShredIndex, 0.1, 10000);
bench_range_density!(bench_density_50_legacy_10k, LegacyShredIndex, 0.5, 10000);
bench_range_density!(bench_density_90_legacy_10k, LegacyShredIndex, 0.9, 10000);

bench_range_density!(bench_density_10_bv, BvShredIndex, 0.1, 1000);
bench_range_density!(bench_density_50_bv, BvShredIndex, 0.5, 1000);
bench_range_density!(bench_density_90_bv, BvShredIndex, 0.9, 1000);
bench_range_density!(bench_density_10_bv_10k, BvShredIndex, 0.1, 10000);
bench_range_density!(bench_density_50_bv_10k, BvShredIndex, 0.5, 10000);
bench_range_density!(bench_density_90_bv_10k, BvShredIndex, 0.9, 10000);

bench_range_density!(bench_density_10_u64, U64ShredIndex, 0.1, 1000);
bench_range_density!(bench_density_50_u64, U64ShredIndex, 0.5, 1000);
bench_range_density!(bench_density_90_u64, U64ShredIndex, 0.9, 1000);
bench_range_density!(bench_density_10_u64_10k, U64ShredIndex, 0.1, 10000);
bench_range_density!(bench_density_50_u64_10k, U64ShredIndex, 0.5, 10000);
bench_range_density!(bench_density_90_u64_10k, U64ShredIndex, 0.9, 10000);

// Distribution pattern benchmarks (sequential, random, clustered) for each implementation
bench_range_distribution!(bench_dist_seq_byte, ByteShredIndex, "sequential");
bench_range_distribution!(bench_dist_rand_byte, ByteShredIndex, "random");
bench_range_distribution!(bench_dist_clust_byte, ByteShredIndex, "clustered");

bench_range_distribution!(bench_dist_seq_bit_array, BitArrayShredIndex, "sequential");
bench_range_distribution!(bench_dist_rand_bit_array, BitArrayShredIndex, "random");
bench_range_distribution!(bench_dist_clust_bit_array, BitArrayShredIndex, "clustered");

bench_range_distribution!(bench_dist_seq_legacy, LegacyShredIndex, "sequential");
bench_range_distribution!(bench_dist_rand_legacy, LegacyShredIndex, "random");
bench_range_distribution!(bench_dist_clust_legacy, LegacyShredIndex, "clustered");

bench_range_distribution!(bench_dist_seq_bv, BvShredIndex, "sequential");
bench_range_distribution!(bench_dist_rand_bv, BvShredIndex, "random");
bench_range_distribution!(bench_dist_clust_bv, BvShredIndex, "clustered");

bench_range_distribution!(bench_dist_seq_u64, U64ShredIndex, "sequential");
bench_range_distribution!(bench_dist_rand_u64, U64ShredIndex, "random");
bench_range_distribution!(bench_dist_clust_u64, U64ShredIndex, "clustered");

// Insertion pattern benchmarks for each implementation
bench_insert_pattern!(bench_insert_seq_byte, ByteShredIndex, "sequential");
bench_insert_pattern!(bench_insert_rand_byte, ByteShredIndex, "random");
bench_insert_pattern!(bench_insert_clust_byte, ByteShredIndex, "clustered");

bench_insert_pattern!(bench_insert_seq_bit_array, BitArrayShredIndex, "sequential");
bench_insert_pattern!(bench_insert_rand_bit_array, BitArrayShredIndex, "random");
bench_insert_pattern!(
    bench_insert_clust_bit_array,
    BitArrayShredIndex,
    "clustered"
);

bench_insert_pattern!(bench_insert_seq_legacy, LegacyShredIndex, "sequential");
bench_insert_pattern!(bench_insert_rand_legacy, LegacyShredIndex, "random");
bench_insert_pattern!(bench_insert_clust_legacy, LegacyShredIndex, "clustered");

bench_insert_pattern!(bench_insert_seq_bv, BvShredIndex, "sequential");
bench_insert_pattern!(bench_insert_rand_bv, BvShredIndex, "random");
bench_insert_pattern!(bench_insert_clust_bv, BvShredIndex, "clustered");

bench_insert_pattern!(bench_insert_seq_u64, U64ShredIndex, "sequential");
bench_insert_pattern!(bench_insert_rand_u64, U64ShredIndex, "random");
bench_insert_pattern!(bench_insert_clust_u64, U64ShredIndex, "clustered");

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! test_serde {
        ($name:ident, $index:ty) => {
            #[test]
            fn $name() {
                let mut index = <$index>::new();
                for i in 0..MAX_DATA_SHREDS_PER_SLOT as u64 {
                    if i % 50 == 0 {
                        index.insert(i);
                    }
                }
                let serialized = bincode::serialize(&index).unwrap();
                let deserialized: $index = bincode::deserialize(&serialized).unwrap();
                assert_eq!(index, deserialized);
            }
        };
    }

    test_serde!(test_serde_byte, ByteShredIndex);
    test_serde!(test_serde_bit_array, BitArrayShredIndex);
    test_serde!(test_serde_legacy, LegacyShredIndex);
    test_serde!(test_serde_bv, BvShredIndex);
    test_serde!(test_serde_u64, U64ShredIndex);

    fn create_indices() -> (
        ByteShredIndex,
        BitArrayShredIndex,
        LegacyShredIndex,
        BvShredIndex,
        U64ShredIndex,
    ) {
        let byte_index = ByteShredIndex::new();
        let bit_index = BitArrayShredIndex::new();
        let legacy_index = LegacyShredIndex::default();
        let bv_index = BvShredIndex::new();
        let u64_index = U64ShredIndex::new();
        (byte_index, bit_index, legacy_index, bv_index, u64_index)
    }

    #[test]
    fn test_basic_insert_contains() {
        let (mut byte_index, mut bit_index, mut legacy_index, mut bv_index, mut u64_index) =
            create_indices();

        let test_indices = [0, 1, 7, 8, 9, 63, 64, 1000, 32767];

        // Test insertions
        for &idx in &test_indices {
            byte_index.insert(idx);
            bit_index.insert(idx);
            legacy_index.insert(idx);
            bv_index.insert(idx);
            u64_index.insert(idx);
        }

        // Test contains
        for idx in 0..MAX_DATA_SHREDS_PER_SLOT as u64 {
            assert_eq!(
                byte_index.contains(idx),
                legacy_index.contains(idx),
                "ByteShredIndex vs Legacy mismatch at index {}",
                idx
            );
            assert_eq!(
                bit_index.contains(idx),
                legacy_index.contains(idx),
                "ShredIndex vs Legacy mismatch at index {}",
                idx
            );
            assert_eq!(
                bv_index.contains(idx),
                legacy_index.contains(idx),
                "BvShredIndex vs Legacy mismatch at index {}",
                idx
            );
            assert_eq!(
                u64_index.contains(idx),
                legacy_index.contains(idx),
                "U64ShredIndex vs Legacy mismatch at index {}",
                idx
            );
        }
    }

    #[test]
    fn test_range_operations() {
        let (mut byte_index, mut bit_index, mut legacy_index, mut bv_index, mut u64_index) =
            create_indices();

        // Insert some patterns
        for i in (0..100).step_by(3) {
            byte_index.insert(i);
            bit_index.insert(i);
            legacy_index.insert(i);
            bv_index.insert(i);
            u64_index.insert(i);
        }

        // Test different range sizes
        let ranges = [0..10, 90..100, 0..1000, 50..150, 32760..32768];

        for range in ranges.iter() {
            let byte_result: Vec<_> = byte_index.range(range.clone()).collect();
            let bit_result: Vec<_> = bit_index.range(range.clone()).collect();
            let legacy_result: Vec<_> = legacy_index.range(range.clone()).copied().collect();
            let bv_result: Vec<_> = bv_index.range(range.clone()).collect();
            let u64_result: Vec<_> = u64_index.range(range.clone()).collect();

            assert_eq!(
                byte_result, legacy_result,
                "ByteShredIndex range results don't match legacy for {:?}",
                range
            );
            assert_eq!(
                bit_result, legacy_result,
                "ShredIndex range results don't match legacy for {:?}",
                range
            );
            assert_eq!(
                bv_result, legacy_result,
                "BvShredIndex range results don't match legacy for {:?}",
                range
            );
            assert_eq!(
                u64_result, legacy_result,
                "U64ShredIndex range results don't match legacy for {:?}",
                range
            );
        }
    }

    #[test]
    fn test_boundary_conditions() {
        let (mut byte_index, mut bit_index, mut legacy_index, mut bv_index, mut u64_index) =
            create_indices();

        let boundaries = [
            MAX_DATA_SHREDS_PER_SLOT as u64 - 2,
            MAX_DATA_SHREDS_PER_SLOT as u64 - 1,
        ];

        for &idx in &boundaries {
            byte_index.insert(idx);
            bit_index.insert(idx);
            legacy_index.insert(idx);
            bv_index.insert(idx);
            u64_index.insert(idx);

            assert_eq!(
                byte_index.contains(idx),
                legacy_index.contains(idx),
                "ByteShredIndex boundary mismatch at {}",
                idx
            );
            assert_eq!(
                bit_index.contains(idx),
                legacy_index.contains(idx),
                "ShredIndex boundary mismatch at {}",
                idx
            );
            assert_eq!(
                bv_index.contains(idx),
                legacy_index.contains(idx),
                "BvShredIndex boundary mismatch at {}",
                idx
            );
            assert_eq!(
                u64_index.contains(idx),
                legacy_index.contains(idx),
                "U64ShredIndex boundary mismatch at {}",
                idx
            );
        }

        // Explicitly test that indices >= MAX_DATA_SHREDS_PER_SLOT are rejected
        let invalid_indices = [
            MAX_DATA_SHREDS_PER_SLOT as u64,
            MAX_DATA_SHREDS_PER_SLOT as u64 + 1,
        ];

        for &idx in &invalid_indices {
            assert!(!byte_index.contains(idx));
            assert!(!bit_index.contains(idx));
            assert!(!bv_index.contains(idx));
            assert!(!u64_index.contains(idx));

            // Insert should have no effect
            byte_index.insert(idx);
            bit_index.insert(idx);
            bv_index.insert(idx);
            u64_index.insert(idx);
            assert!(!byte_index.contains(idx));
            assert!(!bit_index.contains(idx));
            assert!(!bv_index.contains(idx));
            assert!(!u64_index.contains(idx));
        }
    }

    #[test]
    fn test_random_operations() {
        let (mut byte_index, mut bit_index, mut legacy_index, mut bv_index, mut u64_index) =
            create_indices();
        let mut rng = rand::thread_rng();

        // Random insertions
        for _ in 0..1000 {
            let idx = rng.gen_range(0..MAX_DATA_SHREDS_PER_SLOT as u64);
            byte_index.insert(idx);
            bit_index.insert(idx);
            legacy_index.insert(idx);
            bv_index.insert(idx);
            u64_index.insert(idx);
        }

        // Test random ranges
        for _ in 0..100 {
            let start = rng.gen_range(0..MAX_DATA_SHREDS_PER_SLOT as u64);
            let end = rng.gen_range(start..MAX_DATA_SHREDS_PER_SLOT as u64);

            let byte_result: Vec<_> = byte_index.range(start..end).collect();
            let bit_result: Vec<_> = bit_index.range(start..end).collect();
            let legacy_result: Vec<_> = legacy_index.range(start..end).copied().collect();
            let bv_result: Vec<_> = bv_index.range(start..end).collect();
            let u64_result: Vec<_> = u64_index.range(start..end).collect();

            assert_eq!(
                byte_result, legacy_result,
                "ByteShredIndex random range mismatch for {}..{}",
                start, end
            );
            assert_eq!(
                bit_result, legacy_result,
                "ShredIndex random range mismatch for {}..{}",
                start, end
            );
            assert_eq!(
                bv_result, legacy_result,
                "BvShredIndex random range mismatch for {}..{}",
                start, end
            );
            assert_eq!(
                u64_result, legacy_result,
                "U64ShredIndex random range mismatch for {}..{}",
                start, end
            );
        }
    }

    #[test]
    fn test_num_shreds_tracking() {
        let (mut byte_index, mut bit_index, mut legacy_index, mut bv_index, mut u64_index) =
            create_indices();

        // Test duplicate insertions
        for i in 0..100 {
            byte_index.insert(i);
            byte_index.insert(i); // duplicate
            bit_index.insert(i);
            bit_index.insert(i); // duplicate
            legacy_index.insert(i);
            legacy_index.insert(i); // duplicate
            bv_index.insert(i);
            bv_index.insert(i); // duplicate
            u64_index.insert(i);
            u64_index.insert(i); // duplicate
        }

        assert_eq!(
            byte_index.num_shreds,
            legacy_index.num_shreds(),
            "ByteShredIndex num_shreds mismatch after duplicates"
        );
        assert_eq!(
            bit_index.num_shreds,
            legacy_index.num_shreds(),
            "ShredIndex num_shreds mismatch after duplicates"
        );
        assert_eq!(
            bv_index.num_shreds,
            legacy_index.num_shreds(),
            "BvShredIndex num_shreds mismatch after duplicates"
        );
        assert_eq!(
            u64_index.num_shreds,
            legacy_index.num_shreds(),
            "U64ShredIndex num_shreds mismatch after duplicates"
        );
    }

    #[test]
    fn test_edge_case_ranges() {
        let (mut byte_index, mut bit_index, mut legacy_index, mut bv_index, mut u64_index) =
            create_indices();

        // Insert some values near edges
        let edge_cases = [
            0,
            1,
            7,
            8, // First byte boundary
            63,
            64,
            65,                                  // u64 boundary
            MAX_DATA_SHREDS_PER_SLOT as u64 - 2, // End boundary
        ];

        for idx in edge_cases {
            byte_index.insert(idx);
            bit_index.insert(idx);
            legacy_index.insert(idx);
            bv_index.insert(idx);
            u64_index.insert(idx);
        }

        // Test various range patterns
        let ranges = [
            // Start of range
            0..2,
            // Cross u64 boundary
            63..65,
            // Cross byte boundary
            7..9,
            // Full range
            0..MAX_DATA_SHREDS_PER_SLOT as u64,
            // End of range
            (MAX_DATA_SHREDS_PER_SLOT as u64 - 2)..MAX_DATA_SHREDS_PER_SLOT as u64,
        ];

        for range in ranges.iter() {
            let byte_result: Vec<_> = byte_index.range(range.clone()).collect();
            let bit_result: Vec<_> = bit_index.range(range.clone()).collect();
            let legacy_result: Vec<_> = legacy_index.range(range.clone()).copied().collect();
            let bv_result: Vec<_> = bv_index.range(range.clone()).collect();
            let u64_result: Vec<_> = u64_index.range(range.clone()).collect();

            assert_eq!(
                byte_result, legacy_result,
                "ByteShredIndex edge case range mismatch for {:?}",
                range
            );
            assert_eq!(
                bit_result, legacy_result,
                "BitArrayShredIndex edge case range mismatch for {:?}",
                range
            );
            assert_eq!(
                bv_result, legacy_result,
                "BvShredIndex edge case range mismatch for {:?}",
                range
            );
            assert_eq!(
                u64_result, legacy_result,
                "U64ShredIndex edge case range mismatch for {:?}",
                range
            );
        }
    }
}

/// Benchmarks serialization operations with different data densities.
///
/// This test:
/// - Creates a new empty index
/// - Populates it with specified density (1%, 50%, 90%)
/// - Measures the performance impact of data density on serialization
macro_rules! bench_serialize_density {
    ($name:ident, $index:ty, $density:expr) => {
        #[bench]
        fn $name(b: &mut Bencher) {
            let mut index = <$index>::new();
            let mut rng = rand::thread_rng();

            // Calculate number of entries needed for desired density
            let num_entries = (MAX_DATA_SHREDS_PER_SLOT as f64 * $density) as u64;

            // Insert entries to achieve target density
            for _ in 0..num_entries {
                index.insert(rng.gen_range(0..MAX_DATA_SHREDS_PER_SLOT as u64));
            }

            b.iter(|| {
                let serialized = bincode::serialize(&index).unwrap();
                test::black_box(serialized)
            });
        }
    };
}

/// Benchmarks deserialization operations with different data densities.
///
/// This test:
/// - Creates a new empty index
/// - Populates it with specified density (1%, 50%, 90%)
/// - Serializes it once
/// - Measures the performance of repeated deserialization
macro_rules! bench_deserialize_density {
    ($name:ident, $index:ty, $density:expr) => {
        #[bench]
        fn $name(b: &mut Bencher) {
            let mut index = <$index>::new();
            let mut rng = rand::thread_rng();

            // Calculate number of entries needed for desired density
            let num_entries = (MAX_DATA_SHREDS_PER_SLOT as f64 * $density) as u64;

            // Insert entries to achieve target density
            for _ in 0..num_entries {
                index.insert(rng.gen_range(0..MAX_DATA_SHREDS_PER_SLOT as u64));
            }

            let serialized = bincode::serialize(&index).unwrap();

            b.iter(|| {
                let deserialized: $index = bincode::deserialize(&serialized).unwrap();
                test::black_box(deserialized)
            });
        }
    };
}

// Serialization benchmarks for each implementation and density
bench_serialize_density!(bench_serialize_sparse_bytes, ByteShredIndex, 0.01);
bench_serialize_density!(bench_serialize_medium_bytes, ByteShredIndex, 0.5);
bench_serialize_density!(bench_serialize_dense_bytes, ByteShredIndex, 0.9);

bench_serialize_density!(bench_serialize_sparse_bit_array, BitArrayShredIndex, 0.01);
bench_serialize_density!(bench_serialize_medium_bit_array, BitArrayShredIndex, 0.5);
bench_serialize_density!(bench_serialize_dense_bit_array, BitArrayShredIndex, 0.9);

bench_serialize_density!(bench_serialize_sparse_legacy, LegacyShredIndex, 0.01);
bench_serialize_density!(bench_serialize_medium_legacy, LegacyShredIndex, 0.5);
bench_serialize_density!(bench_serialize_dense_legacy, LegacyShredIndex, 0.9);

bench_serialize_density!(bench_serialize_sparse_bv, BvShredIndex, 0.01);
bench_serialize_density!(bench_serialize_medium_bv, BvShredIndex, 0.5);
bench_serialize_density!(bench_serialize_dense_bv, BvShredIndex, 0.9);

bench_serialize_density!(bench_serialize_sparse_u64, U64ShredIndex, 0.01);
bench_serialize_density!(bench_serialize_medium_u64, U64ShredIndex, 0.5);
bench_serialize_density!(bench_serialize_dense_u64, U64ShredIndex, 0.9);

// Deserialization benchmarks for each implementation and density
bench_deserialize_density!(bench_deserialize_sparse_bytes, ByteShredIndex, 0.01);
bench_deserialize_density!(bench_deserialize_medium_bytes, ByteShredIndex, 0.5);
bench_deserialize_density!(bench_deserialize_dense_bytes, ByteShredIndex, 0.9);

bench_deserialize_density!(bench_deserialize_sparse_bit_array, BitArrayShredIndex, 0.01);
bench_deserialize_density!(bench_deserialize_medium_bit_array, BitArrayShredIndex, 0.5);
bench_deserialize_density!(bench_deserialize_dense_bit_array, BitArrayShredIndex, 0.9);

bench_deserialize_density!(bench_deserialize_sparse_legacy, LegacyShredIndex, 0.01);
bench_deserialize_density!(bench_deserialize_medium_legacy, LegacyShredIndex, 0.5);
bench_deserialize_density!(bench_deserialize_dense_legacy, LegacyShredIndex, 0.9);

bench_deserialize_density!(bench_deserialize_sparse_bv, BvShredIndex, 0.01);
bench_deserialize_density!(bench_deserialize_medium_bv, BvShredIndex, 0.5);
bench_deserialize_density!(bench_deserialize_dense_bv, BvShredIndex, 0.9);

bench_deserialize_density!(bench_deserialize_sparse_u64, U64ShredIndex, 0.01);
bench_deserialize_density!(bench_deserialize_medium_u64, U64ShredIndex, 0.5);
bench_deserialize_density!(bench_deserialize_dense_u64, U64ShredIndex, 0.9);
