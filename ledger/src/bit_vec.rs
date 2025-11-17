use {
    serde::{Deserialize, Serialize},
    std::{
        iter::Enumerate,
        ops::{Bound, RangeBounds},
        slice::Iter,
    },
    thiserror::Error,
};

type Word = u8;
const BITS_PER_WORD: usize = std::mem::size_of::<Word>() * 8;

/// A bit vector implementation optimized for efficient bidirectional range
/// scanning and iteration.
///
/// # Examples
///
/// ```
/// # use solana_ledger::bit_vec::BitVec;
/// let mut bit_vec = BitVec::<64>::default();
/// bit_vec.insert(0);
/// bit_vec.insert(1);
/// assert_eq!(bit_vec.range(..2).iter_ones().collect::<Vec<_>>(), [0, 1]);
/// assert_eq!(bit_vec.range(1..).count_ones(), 1);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct BitVec<const NUM_BITS: usize> {
    #[serde(with = "serde_bytes")]
    words: Vec<Word>,
}

impl<const NUM_BITS: usize> Default for BitVec<NUM_BITS> {
    fn default() -> Self {
        Self {
            words: vec![0; Self::NUM_WORDS],
        }
    }
}

// Note: serde_bytes' default `Deserialize` would construct a variable-length buffer,
// which violates `BitVec`'s invariant that its backing vector length (in words)
// is exactly `NUM_WORDS`. Bounds checks and performance rely on this fixed size.
//
// `BitVec` is normally constructed via `Default`, which initializes with
// `vec![0; Self::NUM_WORDS]`. This custom `Deserialize` preserves the invariant by
// allocating exactly `NUM_WORDS` and populating from the serialized data, zero-filling
// any missing words. This is required for the `SlotMetaV1` -> `SlotMetaV2` migration,
// where `completed_data_indexes` was encoded as a variable-length `BTreeSet<u32>`.
impl<'de, const NUM_BITS: usize> Deserialize<'de> for BitVec<NUM_BITS> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let bytes = <&serde_bytes::Bytes as Deserialize>::deserialize(deserializer)?;
        let mut words = Vec::with_capacity(Self::NUM_WORDS);
        words.extend_from_slice(bytes);
        words.resize(Self::NUM_WORDS, 0);

        Ok(Self { words })
    }
}

#[derive(Debug, Error)]
pub enum BitVecError {
    #[error("index out of bounds: {index} >= {num_bits}")]
    OutOfBounds { index: usize, num_bits: usize },
}

impl<const NUM_BITS: usize> BitVec<NUM_BITS> {
    const NUM_WORDS: usize = NUM_BITS.div_ceil(BITS_PER_WORD);

    /// Get the word and bit offset for the given index.
    ///
    /// # Examples
    ///
    /// ```
    /// # use solana_ledger::bit_vec::BitVec;
    /// let (word_idx, bit_idx) = BitVec::<64>::location_of(63);
    /// assert_eq!(word_idx, 7);
    /// assert_eq!(bit_idx, 7);
    /// ```
    pub fn location_of(idx: usize) -> (usize, usize) {
        let word_idx = idx / BITS_PER_WORD;
        let bit_idx = idx & (BITS_PER_WORD - 1);
        (word_idx, bit_idx)
    }

    fn check_bounds(&self, idx: usize) -> Result<(), BitVecError> {
        if idx >= NUM_BITS {
            return Err(BitVecError::OutOfBounds {
                index: idx,
                num_bits: NUM_BITS,
            });
        }
        Ok(())
    }

    /// Remove a bit at the given index.
    ///
    /// Returns `true` if the bit was set, `false` otherwise.
    ///
    /// # Panics
    ///
    /// Panics if the index is out of bounds.
    ///
    /// ```should_panic
    /// # use solana_ledger::bit_vec::BitVec;
    /// let mut bit_vec = BitVec::<64>::default();
    /// bit_vec.remove_unchecked(64);
    /// ```
    ///
    /// # Examples
    ///
    /// ```
    /// # use solana_ledger::bit_vec::BitVec;
    /// let mut bit_vec = BitVec::<64>::default();
    /// assert!(bit_vec.insert_unchecked(63));
    /// assert!(bit_vec.remove_unchecked(63));
    /// assert!(!bit_vec.remove_unchecked(63));
    /// ```
    pub fn remove_unchecked(&mut self, idx: usize) -> bool {
        let (word_idx, bit_idx) = Self::location_of(idx);
        let prev = self.words[word_idx];
        let next = prev & !(1 << bit_idx);
        if prev != next {
            self.words[word_idx] = next;
            true
        } else {
            false
        }
    }

    /// Remove a bit at the given index.
    ///
    /// Returns `true` if the bit was set, `false` otherwise.
    ///
    /// # Errors
    ///
    /// Returns an error if the index is out of bounds.
    ///
    /// ```
    /// # use solana_ledger::bit_vec::BitVec;
    /// let mut bit_vec = BitVec::<64>::default();
    /// assert!(bit_vec.remove(64).is_err());
    /// ```
    ///
    /// # Examples
    ///
    /// ```
    /// # use solana_ledger::bit_vec::BitVec;
    /// let mut bit_vec = BitVec::<64>::default();
    /// assert!(bit_vec.insert_unchecked(63));
    /// assert!(bit_vec.remove(63).is_ok());
    /// assert!(!bit_vec.remove(63).unwrap());
    /// ```
    pub fn remove(&mut self, idx: usize) -> Result<bool, BitVecError> {
        self.check_bounds(idx)?;
        Ok(self.remove_unchecked(idx))
    }

    /// Insert a bit at the given index.
    ///
    /// Returns `true` if the bit was not already set, `false` otherwise.
    ///
    /// # Panics
    ///
    /// Panics if the index is out of bounds.
    ///
    /// ```should_panic
    /// # use solana_ledger::bit_vec::BitVec;
    /// let mut bit_vec = BitVec::<8>::default();
    /// bit_vec.insert_unchecked(64);
    /// ```
    ///
    /// # Examples
    ///
    /// ```
    /// # use solana_ledger::bit_vec::BitVec;
    /// let mut bit_vec = BitVec::<64>::default();
    /// assert!(bit_vec.insert_unchecked(63));
    /// assert!(!bit_vec.insert_unchecked(63));
    /// ```
    pub fn insert_unchecked(&mut self, idx: usize) -> bool {
        let (word_idx, bit_idx) = Self::location_of(idx);
        let prev = self.words[word_idx];
        let next = prev | (1 << bit_idx);
        if prev != next {
            self.words[word_idx] = next;
            true
        } else {
            false
        }
    }

    /// Insert a bit at the given index.
    ///
    /// Returns `true` if the bit was not already set, `false` otherwise.
    ///
    /// # Errors
    ///
    /// Returns an error if the index is out of bounds.
    ///
    /// ```
    /// # use solana_ledger::bit_vec::BitVec;
    /// let mut bit_vec = BitVec::<64>::default();
    /// assert!(bit_vec.insert(64).is_err());
    /// ```
    ///
    /// # Examples
    ///
    /// ```
    /// # use solana_ledger::bit_vec::BitVec;
    /// let mut bit_vec = BitVec::<64>::default();
    /// assert!(bit_vec.insert(63).is_ok());
    /// assert!(!bit_vec.insert(63).unwrap());
    /// ```
    pub fn insert(&mut self, idx: usize) -> Result<bool, BitVecError> {
        self.check_bounds(idx)?;
        Ok(self.insert_unchecked(idx))
    }

    /// Check if a bit is set at the given index.
    ///
    /// # Examples
    ///
    /// ```
    /// # use solana_ledger::bit_vec::BitVec;
    /// let mut bit_vec = BitVec::<64>::default();
    /// bit_vec.insert(63);
    /// assert!(bit_vec.contains(63));
    /// ```
    pub fn contains(&self, idx: usize) -> bool {
        if self.check_bounds(idx).is_err() {
            return false;
        }

        let (word_idx, bit_idx) = Self::location_of(idx);
        (self.words[word_idx] & (1 << bit_idx)) != 0
    }

    /// Get an iterator over the bits in the array within the given range.
    ///
    /// See [`BitVecSlice::from_range_bounds`] for more information.
    ///
    /// # Examples
    ///
    /// ```
    /// # use solana_ledger::bit_vec::BitVec;
    ///
    /// let mut bit_vec = BitVec::<64>::default();
    /// bit_vec.insert(0);
    /// bit_vec.insert(1);
    /// assert_eq!(bit_vec.range(..2).iter_ones().collect::<Vec<_>>(), [0, 1]);
    /// assert_eq!(bit_vec.range(1..).count_ones(), 1);
    /// ```
    pub fn range(&self, bounds: impl RangeBounds<usize>) -> BitVecSlice<'_, NUM_BITS> {
        BitVecSlice::from_range_bounds(self, bounds)
    }

    /// Get an iterator over the positions of the set bits in the array.
    ///
    /// # Examples
    ///
    /// ```
    /// # use solana_ledger::bit_vec::BitVec;
    /// let mut bit_vec = BitVec::<64>::default();
    /// bit_vec.insert(0);
    /// bit_vec.insert(1);
    /// assert_eq!(bit_vec.iter_ones().collect::<Vec<_>>(), [0, 1]);
    /// ```
    pub fn iter_ones(&self) -> impl DoubleEndedIterator<Item = usize> + '_ {
        self.range(..NUM_BITS).iter_ones()
    }

    /// Count the number of set bits in the array.
    ///
    /// # Examples
    ///
    /// ```
    /// # use solana_ledger::bit_vec::BitVec;
    /// let mut bit_vec = BitVec::<64>::default();
    /// bit_vec.insert(0);
    /// bit_vec.insert(1);
    /// assert_eq!(bit_vec.count_ones(), 2);
    /// ```
    pub fn count_ones(&self) -> usize {
        self.range(..NUM_BITS).count_ones()
    }

    /// Shorthand for checking if there are no set bits in the array.
    ///
    /// # Examples
    ///
    /// ```
    /// # use solana_ledger::bit_vec::BitVec;
    /// let mut bit_vec = BitVec::<64>::default();
    /// assert!(bit_vec.is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.count_ones() == 0
    }
}

impl<const NUM_BITS: usize> FromIterator<usize> for BitVec<NUM_BITS> {
    fn from_iter<T: IntoIterator<Item = usize>>(iter: T) -> Self {
        let mut bit_vec = BitVec::<NUM_BITS>::default();
        for idx in iter {
            bit_vec.insert_unchecked(idx);
        }
        bit_vec
    }
}

/// A slice of a [`BitVec`] that provides efficient bit-level iteration.
pub struct BitVecSlice<'a, const NUM_BITS: usize> {
    mask_iter: BitVecMaskIter<'a, NUM_BITS>,
}

impl<'a, const NUM_BITS: usize> BitVecSlice<'a, NUM_BITS> {
    /// Construct a new [`BitVecSlice`] from a [`BitVec`] and a range.
    ///
    /// Internal function -- use [`BitVec::range`].
    fn from_range_bounds(bit_vec: &'a BitVec<NUM_BITS>, bounds: impl RangeBounds<usize>) -> Self {
        let start = match bounds.start_bound() {
            Bound::Included(&n) => n,
            Bound::Excluded(&n) => n + 1,
            Bound::Unbounded => 0,
        }
        .min(NUM_BITS);
        let end = match bounds.end_bound() {
            Bound::Included(&n) => n + 1,
            Bound::Excluded(&n) => n,
            Bound::Unbounded => NUM_BITS,
        }
        .min(NUM_BITS);
        let end_word: usize = end.div_ceil(BITS_PER_WORD);
        let start_word = (start / BITS_PER_WORD).min(end_word);

        Self {
            mask_iter: BitVecMaskIter {
                start,
                end,
                start_word,
                iter: bit_vec.words[start_word..end_word].iter().enumerate(),
            },
        }
    }

    /// Count the number of set bits in the slice.
    ///
    /// # Examples
    ///
    /// ```
    /// # use solana_ledger::bit_vec::BitVec;
    ///
    /// let mut bit_vec = BitVec::<64>::default();
    /// bit_vec.insert(0);
    /// bit_vec.insert(1);
    /// bit_vec.insert(63);
    /// assert_eq!(bit_vec.range(..32).count_ones(), 2);
    /// assert_eq!(bit_vec.range(1..32).count_ones(), 1);
    /// assert_eq!(bit_vec.range(2..32).count_ones(), 0);
    /// assert_eq!(bit_vec.range(32..64).count_ones(), 1);
    /// ```
    pub fn count_ones(self) -> usize {
        self.mask_iter
            .map(|(_, mask)| mask.count_ones() as usize)
            .sum()
    }

    /// Get an iterator over the positions of the set bits in the slice.
    ///
    /// # Examples
    ///
    /// ```
    /// # use solana_ledger::bit_vec::BitVec;
    /// let mut bit_vec = BitVec::<64>::default();
    /// bit_vec.insert(0);
    /// bit_vec.insert(1);
    /// bit_vec.insert(16);
    /// bit_vec.insert(32);
    /// bit_vec.insert(63);
    /// assert_eq!(bit_vec.range(..64).iter_ones().collect::<Vec<_>>(), [0, 1, 16, 32, 63]);
    /// ```
    pub fn iter_ones(self) -> impl DoubleEndedIterator<Item = usize> + 'a {
        self.mask_iter
            .flat_map(|(base_idx, mask)| IterOnes { base_idx, mask })
    }
}

/// An iterator over masked words of a [`BitVecSlice`].
struct BitVecMaskIter<'a, const NUM_BITS: usize> {
    /// The start index of the slice.
    start: usize,
    /// The end index of the slice.
    end: usize,
    /// The starting word index of the slice.
    start_word: usize,
    /// The iterator over the words in the slice.
    iter: Enumerate<Iter<'a, Word>>,
}

impl<const NUM_BITS: usize> BitVecMaskIter<'_, NUM_BITS> {
    /// Compute a mask of checkable bits for the given word at the given index.
    ///
    /// This takes care of excluding bits that are out of bounds relative to the
    /// provided range bounds. For example, bound `0..2` will span a subsection
    /// of the first word. That word may also contain other set bits, so we
    /// mask those out.
    #[inline(always)]
    fn compute_word_mask(&self, word_idx: usize, word: &Word) -> (usize, Word) {
        if word == &0 {
            return (word_idx, 0);
        }
        // Calculate the absolute bit index for this word:
        // - `self.start_word` is from which word in the original array we started
        // - `word_idx` is how many words we've moved through in this slice
        //
        // Example: If we're slicing starting from the second word (start_word = 1)
        // and we're looking at the first word in our slice (word_idx = 0):
        // `base_idx = (1 + 0) * 8 = 8`.
        let base_idx = (self.start_word + word_idx) * BITS_PER_WORD;

        // Calculate which bits we should keep based on slice bounds.
        //
        // Example: If we started our slice at bit 70, and we're looking at the
        // word that contains bits 64-72, we need to mask off bits 64-69.
        let lower_bound = self.start.saturating_sub(base_idx);
        // Similarly, if our slice ends at bit 70, we need to mask off bits 71-72
        let upper_bound = if base_idx + BITS_PER_WORD > self.end {
            self.end - base_idx
        } else {
            BITS_PER_WORD
        };

        // Create and apply the masks to only keep bits within our slice bounds
        let lower_mask = !0 << lower_bound;
        let upper_mask = !0 >> (BITS_PER_WORD - upper_bound);
        (base_idx, word & lower_mask & upper_mask)
    }
}

impl<const NUM_BITS: usize> Iterator for BitVecMaskIter<'_, NUM_BITS> {
    type Item = (usize, Word);

    fn next(&mut self) -> Option<Self::Item> {
        self.iter
            .next()
            .map(|(word_idx, word)| self.compute_word_mask(word_idx, word))
    }
}

impl<const NUM_BITS: usize> DoubleEndedIterator for BitVecMaskIter<'_, NUM_BITS> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.iter
            .next_back()
            .map(|(word_idx, word)| self.compute_word_mask(word_idx, word))
    }
}

struct IterOnes {
    base_idx: usize,
    mask: Word,
}

impl Iterator for IterOnes {
    type Item = usize;

    #[inline(always)]
    fn next(&mut self) -> Option<Self::Item> {
        if self.mask == 0 {
            return None;
        }

        let bit_idx = self.mask.trailing_zeros() as usize;
        // Clear the lowest set bit.
        self.mask &= self.mask - 1;
        // Convert bit position to absolute index by adding word's base_idx.
        Some(self.base_idx + bit_idx)
    }
}

impl DoubleEndedIterator for IterOnes {
    #[inline(always)]
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.mask == 0 {
            return None;
        }

        let bit_idx = BITS_PER_WORD - 1 - self.mask.leading_zeros() as usize;
        // Convert bit position to absolute index by adding word's base_idx.
        let index = self.base_idx + bit_idx;
        // Clear the highest set bit.
        self.mask &= !(1 << bit_idx);

        Some(index)
    }
}

impl ExactSizeIterator for IterOnes {
    fn len(&self) -> usize {
        self.mask.count_ones() as usize
    }
}

#[cfg(test)]
mod tests {
    use {super::*, proptest::prelude::*, std::ops::Range};

    /// Generate a random Range<usize>.
    fn rand_range(range: Range<usize>) -> impl Strategy<Value = Range<usize>> {
        (range.clone(), range).prop_map(
            // Avoid descending (empty) ranges
            |(start, end)| {
                if start > end {
                    end..start
                } else {
                    start..end
                }
            },
        )
    }

    proptest! {
        // Property: insert followed by contains should return true
        #[test]
        fn insert_then_contains(idx in 0..1024_usize) {
            let mut bits = BitVec::<1024>::default();
            prop_assert!(!bits.contains(idx));
            bits.insert_unchecked(idx);
            prop_assert!(bits.contains(idx));
        }

        // Property: insert followed by remove should return true
        #[test]
        fn insert_then_remove(idx in 0..1024_usize) {
            let mut bits = BitVec::<1024>::default();
            prop_assert!(!bits.remove_unchecked(idx));
            bits.insert_unchecked(idx);
            prop_assert!(bits.remove_unchecked(idx));
        }

        // Property: range queries should return correct indices and counts
        #[test]
        fn range_query_correctness(
            range in rand_range(0..1024_usize)
        ) {
            let mut bit_vec = BitVec::<1024>::default();

            for idx in range.clone() {
                bit_vec.insert_unchecked(idx);
            }

            // Test indices match
            prop_assert_eq!(
                bit_vec.range(range.clone()).iter_ones().collect::<Vec<_>>(),
                range.clone().collect::<Vec<_>>()
            );

            // Test count matches
            prop_assert_eq!(
                bit_vec.range(range.clone()).count_ones(),
                range.count()
            );
        }

        // Property: next and next_back should return correct, ordered indices.
        //
        // Additionally, when next or next_back is called when a single word remains,
        // they should be able to operate on the same word.
        #[test]
        fn next_and_next_back_correctness(
            range in rand_range(0..1024_usize)
        ) {
            let mut bit_vec = BitVec::<1024>::default();
            for idx in range.clone() {
                bit_vec.insert_unchecked(idx);
            }

            let mut iter = bit_vec.range(range.clone()).iter_ones();
            for idx in range.clone() {
                prop_assert_eq!(iter.next(), Some(idx));
            }

            let mut iter = bit_vec.range(range.clone()).iter_ones();
            for idx in range.clone().rev() {
                prop_assert_eq!(iter.next_back(), Some(idx));
            }

            let mut iter = bit_vec.range(range.clone()).iter_ones();
            let mut range_iter = range.clone();
            for _ in 0..range.count() {
                prop_assert_eq!(iter.next(), range_iter.next());
                prop_assert_eq!(iter.next_back(), range_iter.next_back());
            }
        }

        #[test]
        fn test_deserialize_from_smaller_length(data in prop::collection::vec(any::<u8>(), 0..BitVec::<1024>::NUM_WORDS)) {
            const NUM_BITS: usize = 1024;
            let mut expected = BitVec::<NUM_BITS>::default();
            expected.words[..data.len()].copy_from_slice(&data);

            #[derive(Serialize)]
            #[serde(transparent)]
            struct Source {
                #[serde(with = "serde_bytes")]
                data: Vec<u8>,
            }
            let serialized = bincode::serialize(&Source { data }).unwrap();
            // Deserializing should always result in a BitVec with exactly NUM_WORDS words,
            // adding zeroed bits that are not present in the serialized data.
            let deserialized: BitVec<NUM_BITS> = bincode::deserialize(&serialized).unwrap();
            prop_assert_eq!(deserialized, expected);
        }

        #[test]
        fn serialize_roundtrip(range in rand_range(0..1024_usize)) {
            const NUM_BITS: usize = 1024;
            let bit_vec = range.into_iter().collect::<BitVec<NUM_BITS>>();
            let serialized = bincode::serialize(&bit_vec).unwrap();
            let deserialized: BitVec<NUM_BITS> = bincode::deserialize(&serialized).unwrap();
            prop_assert_eq!(deserialized, bit_vec);
        }
    }

    #[test]
    fn test_bit_vec_range_bound_combinations() {
        let mut bit_vec = BitVec::<64>::default();

        bit_vec.insert_unchecked(10);
        bit_vec.insert_unchecked(20);
        bit_vec.insert_unchecked(30);
        bit_vec.insert_unchecked(40);

        use std::ops::Bound::*;

        // Test all combinations of bounds
        let test_cases = [
            // (start_bound, end_bound, expected_result)
            (Included(10), Included(30), vec![10, 20, 30]),
            (Included(10), Excluded(30), vec![10, 20]),
            (Excluded(10), Included(30), vec![20, 30]),
            (Excluded(10), Excluded(30), vec![20]),
            // Unbounded start
            (Unbounded, Included(20), vec![10, 20]),
            (Unbounded, Excluded(20), vec![10]),
            // // Unbounded end
            (Included(30), Unbounded, vec![30, 40]),
            (Excluded(30), Unbounded, vec![40]),
            // // Both Unbounded
            (Unbounded, Unbounded, vec![10, 20, 30, 40]),
        ];

        for (start_bound, end_bound, expected) in test_cases {
            let result: Vec<_> = bit_vec
                .range((start_bound, end_bound))
                .iter_ones()
                .collect();
            assert_eq!(
                result, expected,
                "Failed for bounds: start={start_bound:?}, end={end_bound:?}"
            );
        }
    }

    #[test]
    fn test_bit_vec_boundary_conditions() {
        const MAX_BITS: usize = 64;
        let mut bit_vec = BitVec::<MAX_BITS>::default();

        // First possible index
        bit_vec.insert_unchecked(0);
        // Last index in first word (bits 0-7)
        bit_vec.insert_unchecked(7);
        // First index in second word (bits 8-15)
        bit_vec.insert_unchecked(8);
        // Last index in second word
        bit_vec.insert_unchecked(15);
        // Last valid index
        bit_vec.insert_unchecked(MAX_BITS - 1);
        // Should be error (too large)
        assert!(bit_vec.insert(MAX_BITS).is_err());

        // Verify contents
        assert!(bit_vec.contains(0));
        assert!(bit_vec.contains(7));
        assert!(bit_vec.contains(8));
        assert!(bit_vec.contains(15));
        assert!(bit_vec.contains(MAX_BITS - 1));
        assert!(!bit_vec.contains(MAX_BITS));

        // Cross-word boundary
        assert_eq!(
            bit_vec.range(6..10).iter_ones().collect::<Vec<_>>(),
            vec![7, 8]
        );
        // Full first word
        assert_eq!(
            bit_vec.range(0..8).iter_ones().collect::<Vec<_>>(),
            vec![0, 7]
        );
        // Full second word
        assert_eq!(
            bit_vec.range(8..16).iter_ones().collect::<Vec<_>>(),
            vec![8, 15]
        );

        // Empty ranges
        assert_eq!(bit_vec.range(0..0).iter_ones().count(), 0);
        assert_eq!(bit_vec.range(1..1).iter_ones().count(), 0);

        // Test range that exceeds max
        let oversized_range = bit_vec.range(0..MAX_BITS + 1);
        assert_eq!(oversized_range.iter_ones().count(), 5);

        bit_vec.remove_unchecked(0);
        assert!(!bit_vec.contains(0));
        bit_vec.remove_unchecked(7);
        assert!(!bit_vec.contains(7));
        bit_vec.remove_unchecked(8);
        assert!(!bit_vec.contains(8));
        bit_vec.remove_unchecked(15);
        assert!(!bit_vec.contains(15));
        bit_vec.remove_unchecked(MAX_BITS - 1);
        assert!(!bit_vec.contains(MAX_BITS - 1));

        assert_eq!(bit_vec.count_ones(), 0);
    }
}
