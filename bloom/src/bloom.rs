//! Bloom Filter

use {
    bv::BitVec,
    fnv::FnvHasher,
    rand::{self, Rng},
    serde::{Deserialize, Serialize},
    solana_sanitize::{Sanitize, SanitizeError},
    solana_time_utils::AtomicInterval,
    std::{
        cmp, fmt,
        hash::Hasher,
        marker::PhantomData,
        ops::Deref,
        sync::atomic::{AtomicU64, Ordering},
    },
};

/// Generate a stable hash of `self` for each `hash_index`
/// Best effort can be made for uniqueness of each hash.
pub trait BloomHashIndex {
    fn hash_at_index(&self, hash_index: u64) -> u64;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FalsePositiveRate(u8);

impl FalsePositiveRate {
    pub const GOSSIP_10_PERCENT: FalsePositiveRate = FalsePositiveRate::new(10);

    const FRACTIONAL_BITS: u32 = 40;
    pub const ONE_Q40: u64 = 1u64 << Self::FRACTIONAL_BITS;

    // Q40 form of the legacy float for `p = 0.1` and `k = 8` hash functions: c = ln(1 - p^(1/k)) / (-k) ~= 0.1732339109947
    // round(c * 2^40) = 190_472_699_464.
    pub const MAX_ITEMS_Q40_P10_PERCENT: u64 = 190_472_699_464;

    /// Intentionally cap at 20% due to high noise level.
    const MAX_FALSE_POSITIVE_RATE: usize = 20;

    /// `ln(2)` represented in Q40 fixed-point: `round(ln(2) * 2^40)`.
    ///
    /// This lets `num_keys()` preserve the historical
    /// `round((num_bits / num_items) * ln(2))` formula without runtime floats.
    const LN_2_Q40: u64 = 762_123_384_786;

    /// Precomputed `ceil((-ln(p) / ln(2)^2) * 2^40)` for integer percentages
    /// `p` in `1..=20`. Look at test `regenerate_bits_per_item_q40_table`
    ///
    /// This replaces runtime floating-point math in `num_bits()` while preserving
    /// the same results as the historical `ceil(n * (-ln(p) / ln(2)^2))`
    ///
    /// We store values in Q40 fixed-point so the runtime code can recover
    /// the integer and fractional parts using only `u64` arithmetic. The table is
    /// used instead of computing logarithms on the fly to avoid floats in the
    /// protocol path.
    const BITS_PER_ITEM_Q40: [u64; Self::MAX_FALSE_POSITIVE_RATE] = [
        10538883138827,
        8952623166035,
        8024720565557,
        7366363193243,
        6855701542206,
        6438460592764,
        6085688396546,
        5780103220451,
        5510557992286,
        5269441569414,
        5051325233571,
        4852200619972,
        4669023732210,
        4499428423754,
        4341538968935,
        4193843247659,
        4055104443476,
        3924298019494,
        3800565756929,
        3683181596621,
    ];

    /// Creates false-positive rate in percent. Capped 20%.
    pub const fn new(percent: u8) -> Self {
        assert!(
            percent > 0 && percent <= Self::MAX_FALSE_POSITIVE_RATE as u8,
            "false positive rate must be in 1..=20"
        );
        Self(percent)
    }

    /// Returns precomputed `-ln(p) / ln(2)^2` in Q40 fixed-point
    /// for false-positive rate `p`.
    fn bits_per_item_q40(self) -> u64 {
        Self::BITS_PER_ITEM_Q40[usize::from(self.0 - 1)]
    }
}

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Eq)]
pub struct Bloom<T: BloomHashIndex> {
    pub keys: Vec<u64>,
    pub bits: BitVec<u64>,
    num_bits_set: u64,
    _phantom: PhantomData<T>,
}

impl<T: BloomHashIndex> fmt::Debug for Bloom<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Bloom {{ keys.len: {} bits.len: {} num_set: {} bits: ",
            self.keys.len(),
            self.bits.len(),
            self.num_bits_set
        )?;
        const MAX_PRINT_BITS: u64 = 10;
        for i in 0..std::cmp::min(MAX_PRINT_BITS, self.bits.len()) {
            if self.bits.get(i) {
                write!(f, "1")?;
            } else {
                write!(f, "0")?;
            }
        }
        if self.bits.len() > MAX_PRINT_BITS {
            write!(f, "..")?;
        }
        write!(f, " }}")
    }
}

impl<T: BloomHashIndex> Sanitize for Bloom<T> {
    fn sanitize(&self) -> Result<(), SanitizeError> {
        // Avoid division by zero in self.pos(...).
        if self.bits.is_empty() {
            Err(SanitizeError::InvalidValue)
        } else {
            Ok(())
        }
    }
}

impl<T: BloomHashIndex> Bloom<T> {
    pub fn new(num_bits: usize, keys: Vec<u64>) -> Self {
        let bits = BitVec::new_fill(false, num_bits as u64);
        Bloom {
            keys,
            bits,
            num_bits_set: 0,
            _phantom: PhantomData,
        }
    }
    /// Create filter optimal for num size given the false positive rate.
    ///
    /// The keys are randomized for picking data out of a collision resistant hash of size
    /// `keysize` bytes.
    ///
    /// See <https://hur.st/bloomfilter/>.
    pub fn random(
        num_items: usize,
        false_positive_rate: FalsePositiveRate,
        max_bits: usize,
    ) -> Self {
        let m = Self::num_bits(num_items as u64, false_positive_rate);
        let num_bits = cmp::max(1, cmp::min(usize::try_from(m).unwrap(), max_bits));
        let num_keys = usize::try_from(Self::num_keys(num_bits as u64, num_items as u64)).unwrap();
        let keys: Vec<u64> = (0..num_keys).map(|_| rand::rng().random()).collect();
        Self::new(num_bits, keys)
    }
    fn div_ceil(numerator: u64, denominator: u64) -> u64 {
        numerator.div_ceil(denominator)
    }
    fn div_round(numerator: u64, denominator: u64) -> u64 {
        assert!(denominator != 0, "div_round denominator must be non-zero");
        let quotient = numerator / denominator;
        let remainder = numerator % denominator;
        quotient + u64::from(remainder >= denominator.div_ceil(2))
    }

    /// Computes `ceil(num_items * bits_per_item(false_rate))` in Q40 fixed-point.
    ///
    /// Not valid for all `num_items`: this function panics on checked overflow.
    /// For a given `false_positive_rate`, a tight bound comes from the fractional multiply:
    /// `num_items <= u64::MAX / fractional_part`, where
    /// `fractional_part = bits_per_item_q40 & (ONE_Q40 - 1)`.
    fn num_bits(num_items: u64, false_positive_rate: FalsePositiveRate) -> u64 {
        let bits_per_item_q40 = false_positive_rate.bits_per_item_q40();
        let integer_part = bits_per_item_q40 >> FalsePositiveRate::FRACTIONAL_BITS;
        let fractional_part = bits_per_item_q40 & (FalsePositiveRate::ONE_Q40 - 1);
        let whole_bits = num_items
            .checked_mul(integer_part)
            .expect("num_bits overflow: whole_bits");
        let fractional_bits = Self::div_ceil(
            num_items
                .checked_mul(fractional_part)
                .expect("num_bits overflow: fractional_bits"),
            FalsePositiveRate::ONE_Q40,
        );
        whole_bits
            .checked_add(fractional_bits)
            .expect("num_bits overflow: total_bits")
    }
    fn num_keys(num_bits: u64, num_items: u64) -> u64 {
        if num_items == 0 {
            0
        } else {
            let whole = num_bits / num_items;
            let remainder = num_bits % num_items;
            let whole_q40 = whole
                .checked_mul(FalsePositiveRate::LN_2_Q40)
                .expect("num_keys overflow: whole_q40");
            let remainder_q40 = Self::div_round(
                remainder
                    .checked_mul(FalsePositiveRate::LN_2_Q40)
                    .expect("num_keys overflow: remainder_q40"),
                num_items,
            );
            let ratio_q40 = whole_q40
                .checked_add(remainder_q40)
                .expect("num_keys overflow: ratio_q40");
            u64::max(1, Self::div_round(ratio_q40, FalsePositiveRate::ONE_Q40))
        }
    }
    fn pos(&self, key: &T, k: u64) -> u64 {
        key.hash_at_index(k)
            .checked_rem(self.bits.len())
            .unwrap_or(0)
    }
    pub fn clear(&mut self) {
        self.bits = BitVec::new_fill(false, self.bits.len());
        self.num_bits_set = 0;
    }
    pub fn add(&mut self, key: &T) {
        for k in &self.keys {
            let pos = self.pos(key, *k);
            if !self.bits.get(pos) {
                self.num_bits_set = self.num_bits_set.saturating_add(1);
                self.bits.set(pos, true);
            }
        }
    }
    pub fn contains(&self, key: &T) -> bool {
        for k in &self.keys {
            let pos = self.pos(key, *k);
            if !self.bits.get(pos) {
                return false;
            }
        }
        true
    }
}

fn slice_hash(slice: &[u8], hash_index: u64) -> u64 {
    let mut hasher = FnvHasher::with_key(hash_index);
    hasher.write(slice);
    hasher.finish()
}

impl<T: AsRef<[u8]>> BloomHashIndex for T {
    fn hash_at_index(&self, hash_index: u64) -> u64 {
        slice_hash(self.as_ref(), hash_index)
    }
}

/// Bloom filter that can be used concurrently.
/// Concurrent reads/writes are safe, but are not atomic at the struct level,
/// this means that reads may see partial writes.
pub struct ConcurrentBloom<T> {
    num_bits: u64,
    keys: Vec<u64>,
    bits: Vec<AtomicU64>,
    _phantom: PhantomData<T>,
}

impl<T: BloomHashIndex> From<Bloom<T>> for ConcurrentBloom<T> {
    fn from(bloom: Bloom<T>) -> Self {
        ConcurrentBloom {
            num_bits: bloom.bits.len(),
            keys: bloom.keys,
            bits: bloom
                .bits
                .into_boxed_slice()
                .iter()
                .map(|&x| AtomicU64::new(x))
                .collect(),
            _phantom: PhantomData,
        }
    }
}

impl<T: BloomHashIndex> ConcurrentBloom<T> {
    fn pos(&self, key: &T, hash_index: u64) -> (usize, u64) {
        let pos = key
            .hash_at_index(hash_index)
            .checked_rem(self.num_bits)
            .unwrap_or(0);
        // Divide by 64 to figure out which of the
        // AtomicU64 bit chunks we need to modify.
        let index = pos.wrapping_shr(6);
        // (pos & 63) is equivalent to mod 64 so that we can find
        // the index of the bit within the AtomicU64 to modify.
        let mask = 1u64.wrapping_shl(u32::try_from(pos & 63).unwrap());
        (index as usize, mask)
    }

    /// Adds an item to the bloom filter and returns true if the item
    /// was not in the filter before.
    pub fn add(&self, key: &T) -> bool {
        let mut added = false;
        for k in &self.keys {
            let (index, mask) = self.pos(key, *k);
            let prev_val = self.bits[index].fetch_or(mask, Ordering::Relaxed);
            added = added || prev_val & mask == 0u64;
        }
        added
    }

    pub fn contains(&self, key: &T) -> bool {
        self.keys.iter().all(|k| {
            let (index, mask) = self.pos(key, *k);
            let bit = self.bits[index].load(Ordering::Relaxed) & mask;
            bit != 0u64
        })
    }

    pub fn clear(&self) {
        self.bits.iter().for_each(|bit| {
            bit.store(0u64, Ordering::Relaxed);
        });
    }
}

impl<T: BloomHashIndex> From<ConcurrentBloom<T>> for Bloom<T> {
    fn from(atomic_bloom: ConcurrentBloom<T>) -> Self {
        let bits: Vec<_> = atomic_bloom
            .bits
            .into_iter()
            .map(AtomicU64::into_inner)
            .collect();
        let num_bits_set = bits.iter().map(|x| x.count_ones() as u64).sum();
        let mut bits: BitVec<u64> = bits.into();
        bits.truncate(atomic_bloom.num_bits);
        Bloom {
            keys: atomic_bloom.keys,
            bits,
            num_bits_set,
            _phantom: PhantomData,
        }
    }
}

/// Wrapper around `ConcurrentBloom` and `AtomicInterval` so the bloom filter
/// can be cleared periodically.
pub struct ConcurrentBloomInterval<T: BloomHashIndex> {
    interval: AtomicInterval,
    bloom: ConcurrentBloom<T>,
}

// Directly allow all methods of `AtomicBloom` to be called on `AtomicBloomInterval`.
impl<T: BloomHashIndex> Deref for ConcurrentBloomInterval<T> {
    type Target = ConcurrentBloom<T>;
    fn deref(&self) -> &Self::Target {
        &self.bloom
    }
}

impl<T: BloomHashIndex> ConcurrentBloomInterval<T> {
    /// Create a new filter with the given parameters.
    /// See `Bloom::random` for details.
    pub fn new(num_items: usize, false_positive_rate: FalsePositiveRate, max_bits: usize) -> Self {
        let bloom = Bloom::random(num_items, false_positive_rate, max_bits);
        Self {
            interval: AtomicInterval::default(),
            bloom: ConcurrentBloom::from(bloom),
        }
    }

    /// Reset the filter if the reset interval has elapsed.
    pub fn maybe_reset(&self, reset_interval_ms: u64) {
        if self.interval.should_update(reset_interval_ms) {
            self.bloom.clear();
        }
    }
}

#[cfg(test)]
mod test {
    use {super::*, rayon::prelude::*, solana_hash::Hash, solana_sha256_hasher::hash};

    #[test]
    fn test_bloom_filter() {
        //empty
        let bloom: Bloom<Hash> = Bloom::random(0, FalsePositiveRate::new(10), 100);
        assert_eq!(bloom.keys.len(), 0);
        assert_eq!(bloom.bits.len(), 1);

        //normal
        let bloom: Bloom<Hash> = Bloom::random(10, FalsePositiveRate::new(10), 100);
        assert_eq!(bloom.keys.len(), 3);
        assert_eq!(bloom.bits.len(), 48);

        //saturated
        let bloom: Bloom<Hash> = Bloom::random(100, FalsePositiveRate::new(10), 100);
        assert_eq!(bloom.keys.len(), 1);
        assert_eq!(bloom.bits.len(), 100);
    }
    #[test]
    fn test_add_contains() {
        let mut bloom: Bloom<Hash> = Bloom::random(100, FalsePositiveRate::new(10), 100);
        //known keys to avoid false positives in the test
        bloom.keys = vec![0, 1, 2, 3];

        let key = hash(b"hello");
        assert!(!bloom.contains(&key));
        bloom.add(&key);
        assert!(bloom.contains(&key));

        let key = hash(b"world");
        assert!(!bloom.contains(&key));
        bloom.add(&key);
        assert!(bloom.contains(&key));
    }
    #[test]
    fn test_random() {
        let mut b1: Bloom<Hash> = Bloom::random(10, FalsePositiveRate::new(10), 100);
        let mut b2: Bloom<Hash> = Bloom::random(10, FalsePositiveRate::new(10), 100);
        b1.keys.sort_unstable();
        b2.keys.sort_unstable();
        assert_ne!(b1.keys, b2.keys);
    }

    #[test]
    #[should_panic(expected = "num_bits overflow: whole_bits")]
    fn test_num_bits_panics_on_overflow() {
        let _ = Bloom::<Hash>::num_bits(u64::MAX, FalsePositiveRate::new(1));
    }

    #[test]
    #[should_panic(expected = "num_keys overflow: whole_q40")]
    fn test_num_keys_panics_on_overflow() {
        let _ = Bloom::<Hash>::num_keys(u64::MAX, 1);
    }

    #[test]
    fn test_num_keys_guard_for_zero_items() {
        assert_eq!(Bloom::<Hash>::num_keys(u64::MAX, 0), 0);
    }

    #[test]
    #[should_panic(expected = "num_bits overflow: fractional_bits")]
    fn test_random_panics_above_num_items_bound() {
        let false_rate = FalsePositiveRate::new(15);
        let bits_per_item_q40 = false_rate.bits_per_item_q40();
        let fractional_part = bits_per_item_q40 & (FalsePositiveRate::ONE_Q40 - 1);
        let max_items = u64::MAX / fractional_part;
        let too_many_items = max_items + 1;
        let _ = Bloom::<Hash>::random(usize::try_from(too_many_items).unwrap(), false_rate, 1024);
    }

    // Bloom filter math in python
    // n number of items
    // p false rate
    // m number of bits
    // k number of keys
    //
    // n = ceil(m / (-k / log(1 - exp(log(p) / k))))
    // p = pow(1 - exp(-k / (m / n)), k)
    // m = ceil((n * log(p)) / log(1 / pow(2, log(2))));
    // k = round((m / n) * log(2));
    #[test]
    fn test_filter_math() {
        assert_eq!(
            Bloom::<Hash>::num_bits(100, FalsePositiveRate::new(10)),
            480u64
        );
        assert_eq!(
            Bloom::<Hash>::num_bits(100, FalsePositiveRate::new(1)),
            959u64
        );
        assert_eq!(Bloom::<Hash>::num_keys(1000, 50), 14u64);
        assert_eq!(Bloom::<Hash>::num_keys(2000, 50), 28u64);
        assert_eq!(Bloom::<Hash>::num_keys(2000, 25), 55u64);
        //ensure min keys is 1
        assert_eq!(Bloom::<Hash>::num_keys(20, 1000), 1u64);
    }

    #[test]
    #[ignore = "helper: generate FalsePositiveRate::BITS_PER_ITEM_Q40 table"]
    fn regenerate_bits_per_item_q40_table() {
        let ln2_squared = std::f64::consts::LN_2 * std::f64::consts::LN_2;
        let scale = FalsePositiveRate::ONE_Q40 as f64;
        let generated: Vec<u64> = (1..=FalsePositiveRate::MAX_FALSE_POSITIVE_RATE)
            .map(|percent| {
                let probability = percent as f64 / 100.0;
                let coefficient = -probability.ln() / ln2_squared;
                (coefficient * scale).round() as u64
            })
            .collect();

        assert_eq!(generated, FalsePositiveRate::BITS_PER_ITEM_Q40.to_vec());

        println!(
            "const BITS_PER_ITEM_Q40: [u64; {}] = [",
            FalsePositiveRate::MAX_FALSE_POSITIVE_RATE
        );
        for value in generated {
            println!("    {value},");
        }
        println!("];");
    }

    #[test]
    fn test_bloom_wire_format_regression() {
        // Golden values below are extracted from `master` commit:
        // 85c24be0856a28f8d94002d56081c722732b742d
        fn assert_wire_format(
            num_items: usize,
            false_rate: FalsePositiveRate,
            max_bits: usize,
            expected_num_bits: u64,
            expected_num_keys: u64,
            keys: Vec<u64>,
            expected_serialized_len: usize,
            expected_serialized_hash: Hash,
        ) {
            let unclamped_num_bits = Bloom::<Hash>::num_bits(num_items as u64, false_rate);
            let num_bits = u64::max(1, unclamped_num_bits.min(max_bits as u64));
            let num_keys = Bloom::<Hash>::num_keys(num_bits, num_items as u64);
            assert_eq!(num_bits, expected_num_bits);
            assert_eq!(num_keys, expected_num_keys);

            let random_bloom = Bloom::<Hash>::random(num_items, false_rate, max_bits);
            assert_eq!(random_bloom.bits.len(), expected_num_bits);
            assert_eq!(random_bloom.keys.len(), expected_num_keys as usize);

            let mut bloom = Bloom::<Hash>::new(num_bits as usize, keys);
            for hash_value in [
                Hash::new_from_array([0u8; 32]),
                Hash::new_from_array([1u8; 32]),
                Hash::new_from_array([2u8; 32]),
                Hash::new_from_array([3u8; 32]),
                Hash::new_from_array([4u8; 32]),
            ] {
                bloom.add(&hash_value);
            }

            let serialized = bincode::serialize(&bloom).unwrap();
            assert_eq!(serialized.len(), expected_serialized_len);
            assert_eq!(hash(serialized.as_slice()), expected_serialized_hash);
        }

        assert_wire_format(
            1287,
            FalsePositiveRate::new(10),
            7424,
            6168,
            3,
            vec![
                0x0123_4567_89ab_cdef,
                0xfedc_ba98_7654_3210,
                0x0f1e_2d3c_4b5a_6978,
            ],
            833,
            Hash::new_from_array([
                186, 247, 46, 104, 13, 127, 226, 6, 196, 199, 126, 11, 99, 173, 236, 66, 163, 10,
                228, 233, 220, 127, 121, 247, 12, 183, 173, 231, 122, 182, 112, 121,
            ]),
        );
        assert_wire_format(
            1287,
            FalsePositiveRate::new(1),
            20000,
            12336,
            7,
            vec![
                0x0123_4567_89ab_cdef,
                0xfedc_ba98_7654_3210,
                0x0f1e_2d3c_4b5a_6978,
                0x8877_6655_4433_2211,
                0x1122_3344_5566_7788,
            ],
            1617,
            Hash::new_from_array([
                116, 127, 147, 126, 135, 69, 139, 180, 8, 181, 101, 161, 178, 175, 6, 144, 48, 13,
                38, 26, 175, 55, 44, 225, 5, 207, 86, 162, 167, 141, 173, 100,
            ]),
        );
        assert_wire_format(
            1287,
            FalsePositiveRate::new(20),
            7424,
            4312,
            2,
            vec![0x0123_4567_89ab_cdef, 0xfedc_ba98_7654_3210],
            593,
            Hash::new_from_array([
                215, 164, 43, 80, 62, 66, 140, 249, 52, 108, 205, 159, 65, 208, 130, 87, 44, 238,
                34, 111, 156, 150, 69, 175, 36, 53, 134, 26, 101, 100, 1, 47,
            ]),
        );
        assert_wire_format(
            1,
            FalsePositiveRate::new(20),
            7424,
            4,
            3,
            vec![
                0x0123_4567_89ab_cdef,
                0xfedc_ba98_7654_3210,
                0x0f1e_2d3c_4b5a_6978,
            ],
            65,
            Hash::new_from_array([
                217, 252, 184, 119, 19, 21, 177, 234, 254, 84, 98, 71, 38, 44, 13, 216, 67, 137,
                203, 180, 135, 41, 61, 238, 39, 92, 187, 231, 214, 121, 211, 14,
            ]),
        );
        assert_wire_format(
            1287,
            FalsePositiveRate::new(1),
            7424,
            7424,
            4,
            vec![
                0x0123_4567_89ab_cdef,
                0xfedc_ba98_7654_3210,
                0x0f1e_2d3c_4b5a_6978,
                0x8877_6655_4433_2211,
            ],
            993,
            Hash::new_from_array([
                60, 90, 47, 203, 97, 70, 158, 220, 23, 242, 249, 104, 70, 43, 230, 111, 195, 239,
                9, 11, 201, 255, 104, 13, 185, 74, 7, 248, 195, 178, 72, 208,
            ]),
        );
    }

    #[test]
    fn test_debug() {
        let mut b: Bloom<Hash> = Bloom::new(3, vec![100]);
        b.add(&Hash::default());
        assert_eq!(
            format!("{b:?}"),
            "Bloom { keys.len: 1 bits.len: 3 num_set: 1 bits: 001 }"
        );

        let mut b: Bloom<Hash> = Bloom::new(1000, vec![100]);
        b.add(&Hash::default());
        b.add(&hash(&[1, 2]));
        assert_eq!(
            format!("{b:?}"),
            "Bloom { keys.len: 1 bits.len: 1000 num_set: 2 bits: 0000000000.. }"
        );
    }

    fn generate_random_hash() -> Hash {
        let mut rng = rand::rng();
        let mut hash = [0u8; solana_hash::HASH_BYTES];
        rng.fill(&mut hash);
        Hash::new_from_array(hash)
    }

    #[test]
    fn test_atomic_bloom() {
        let hash_values: Vec<_> = std::iter::repeat_with(generate_random_hash)
            .take(1200)
            .collect();
        let bloom: ConcurrentBloom<_> =
            Bloom::<Hash>::random(1287, FalsePositiveRate::new(10), 7424).into();
        assert_eq!(bloom.keys.len(), 3);
        assert_eq!(bloom.num_bits, 6168);
        assert_eq!(bloom.bits.len(), 97);
        hash_values.par_iter().for_each(|v| {
            bloom.add(v);
        });
        let bloom: Bloom<Hash> = bloom.into();
        assert_eq!(bloom.keys.len(), 3);
        assert_eq!(bloom.bits.len(), 6168);
        assert!(bloom.num_bits_set > 2000);
        for hash_value in hash_values {
            assert!(bloom.contains(&hash_value));
        }
        let false_positive = std::iter::repeat_with(generate_random_hash)
            .take(10_000)
            .filter(|hash_value| bloom.contains(hash_value))
            .count();
        assert!(false_positive < 2_000, "false_positive: {false_positive}");
    }

    #[test]
    fn test_atomic_bloom_round_trip() {
        let mut rng = rand::rng();
        let keys: Vec<_> = std::iter::repeat_with(|| rng.random()).take(5).collect();
        let mut bloom = Bloom::<Hash>::new(9731, keys.clone());
        let hash_values: Vec<_> = std::iter::repeat_with(generate_random_hash)
            .take(1000)
            .collect();
        for hash_value in &hash_values {
            bloom.add(hash_value);
        }
        let num_bits_set = bloom.num_bits_set;
        assert!(num_bits_set > 2000, "# bits set: {num_bits_set}");
        // Round-trip with no inserts.
        let bloom: ConcurrentBloom<_> = bloom.into();
        assert_eq!(bloom.num_bits, 9731);
        assert_eq!(bloom.bits.len(), 9731_usize.div_ceil(64));
        for hash_value in &hash_values {
            assert!(bloom.contains(hash_value));
        }
        let bloom: Bloom<_> = bloom.into();
        assert_eq!(bloom.num_bits_set, num_bits_set);
        for hash_value in &hash_values {
            assert!(bloom.contains(hash_value));
        }
        // Round trip, re-inserting the same hash values.
        let bloom: ConcurrentBloom<_> = bloom.into();
        hash_values.par_iter().for_each(|v| {
            bloom.add(v);
        });
        for hash_value in &hash_values {
            assert!(bloom.contains(hash_value));
        }
        let bloom: Bloom<_> = bloom.into();
        assert_eq!(bloom.num_bits_set, num_bits_set);
        assert_eq!(bloom.bits.len(), 9731);
        for hash_value in &hash_values {
            assert!(bloom.contains(hash_value));
        }
        // Round trip, inserting new hash values.
        let more_hash_values: Vec<_> = std::iter::repeat_with(generate_random_hash)
            .take(1000)
            .collect();
        let bloom: ConcurrentBloom<_> = bloom.into();
        assert_eq!(bloom.num_bits, 9731);
        assert_eq!(bloom.bits.len(), 9731_usize.div_ceil(64));
        more_hash_values.par_iter().for_each(|v| {
            bloom.add(v);
        });
        for hash_value in &hash_values {
            assert!(bloom.contains(hash_value));
        }
        for hash_value in &more_hash_values {
            assert!(bloom.contains(hash_value));
        }
        let false_positive = std::iter::repeat_with(generate_random_hash)
            .take(10_000)
            .filter(|hash_value| bloom.contains(hash_value))
            .count();
        assert!(false_positive < 2000, "false_positive: {false_positive}");
        let bloom: Bloom<_> = bloom.into();
        assert_eq!(bloom.bits.len(), 9731);
        assert!(bloom.num_bits_set > num_bits_set);
        assert!(
            bloom.num_bits_set > 4000,
            "# bits set: {}",
            bloom.num_bits_set
        );
        for hash_value in &hash_values {
            assert!(bloom.contains(hash_value));
        }
        for hash_value in &more_hash_values {
            assert!(bloom.contains(hash_value));
        }
        let false_positive = std::iter::repeat_with(generate_random_hash)
            .take(10_000)
            .filter(|hash_value| bloom.contains(hash_value))
            .count();
        assert!(false_positive < 2000, "false_positive: {false_positive}");
        // Assert that the bits vector precisely match if no atomic ops were
        // used.
        let bits = bloom.bits;
        let mut bloom = Bloom::<Hash>::new(9731, keys);
        for hash_value in &hash_values {
            bloom.add(hash_value);
        }
        for hash_value in &more_hash_values {
            bloom.add(hash_value);
        }
        assert_eq!(bits, bloom.bits);
    }
}
