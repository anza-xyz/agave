//! The `weighted_shuffle` module provides an iterator over shuffled weights.

use {
    num_traits::{CheckedAdd, ConstZero},
    rand::{
        distributions::uniform::{SampleUniform, UniformSampler},
        Rng,
    },
    std::{
        borrow::Borrow,
        ops::{AddAssign, SubAssign},
    },
};

// Each internal tree node has FANOUT many child nodes with indices:
//     (index << BIT_SHIFT) + 1 ..= (index << BIT_SHIFT) + FANOUT
// Conversely, for each node, the parent node is obtained by:
//     parent: (index - 1) >> BIT_SHIFT
// and the subtree weight is stored at
//     offset: (index - 1) & BIT_MASK
// of its parent node.
const BIT_SHIFT: usize = 4;
const FANOUT: usize = 1 << BIT_SHIFT;
const BIT_MASK: usize = FANOUT - 1;

/// Implements an iterator where indices are shuffled according to their
/// weights:
///   - Returned indices are unique in the range [0, weights.len()).
///   - Higher weighted indices tend to appear earlier proportional to their
///     weight.
///   - Zero weighted indices are shuffled and appear only at the end, after
///     non-zero weighted indices.
pub struct WeightedShuffle<T> {
    // Number of "internal" nodes of the tree.
    num_nodes: usize,
    // Underlying array implementing the tree.
    // Nodes without children are never accessed and don't need to be
    // allocated, so tree.len() < num_nodes.
    // tree[i][j] is the sum of all weights in the j'th sub-tree of node i.
    tree: Vec<[T; FANOUT]>,
    // Current sum of all weights, excluding already sampled ones.
    weight: T,
    // Indices of zero weighted entries.
    zeros: Vec<usize>,
}

impl<T: ConstZero> WeightedShuffle<T> {
    const ZERO: T = <T as ConstZero>::ZERO;
}

impl<T> WeightedShuffle<T>
where
    T: Copy + ConstZero + PartialOrd + AddAssign + CheckedAdd,
{
    /// If weights are negative or overflow the total sum
    /// they are treated as zero.
    pub fn new<I>(name: &'static str, weights: I) -> Self
    where
        I: IntoIterator<Item: Borrow<T>>,
        <I as IntoIterator>::IntoIter: ExactSizeIterator,
    {
        let weights = weights.into_iter();
        let (num_nodes, size) = get_num_nodes_and_tree_size(weights.len());
        debug_assert!(size <= num_nodes);
        let mut tree = vec![[Self::ZERO; FANOUT]; size];
        let mut sum = Self::ZERO;
        let mut zeros = Vec::default();
        let mut num_negative: usize = 0;
        let mut num_overflow: usize = 0;
        for (k, weight) in weights.enumerate() {
            let weight = *weight.borrow();
            #[allow(clippy::neg_cmp_op_on_partial_ord)]
            // weight < zero does not work for NaNs.
            if !(weight >= Self::ZERO) {
                zeros.push(k);
                num_negative += 1;
                continue;
            }
            if weight == Self::ZERO {
                zeros.push(k);
                continue;
            }
            sum = match sum.checked_add(&weight) {
                Some(val) => val,
                None => {
                    zeros.push(k);
                    num_overflow += 1;
                    continue;
                }
            };
            // Traverse the tree from the leaf node upwards to the root,
            // updating the sub-tree sums along the way.
            let mut index = num_nodes + k; // leaf node
            while index != 0 {
                let offset = (index - 1) & BIT_MASK;
                index = (index - 1) >> BIT_SHIFT; // parent node
                debug_assert!(index < tree.len());
                // SAFETY: Index is updated to a lesser value towards zero.
                // The bitwise AND operation with BIT_MASK ensures that offset
                // is always less than FANOUT, which is the size of the inner
                // arrays. As a result, tree[index][offset] never goes out of
                // bounds.
                unsafe { tree.get_unchecked_mut(index).get_unchecked_mut(offset) }
                    .add_assign(weight);
            }
        }
        if num_negative > 0 {
            datapoint_error!("weighted-shuffle-negative", (name, num_negative, i64));
        }
        if num_overflow > 0 {
            datapoint_error!("weighted-shuffle-overflow", (name, num_overflow, i64));
        }
        Self {
            num_nodes,
            tree,
            weight: sum,
            zeros,
        }
    }
}

impl<T> WeightedShuffle<T>
where
    T: Copy + ConstZero + PartialOrd + SubAssign,
{
    // Removes given weight at index k.
    fn remove(&mut self, k: usize, weight: T) {
        debug_assert!(self.weight >= weight);
        self.weight -= weight;
        // Traverse the tree from the leaf node upwards to the root,
        // updating the sub-tree sums along the way.
        let mut index = self.num_nodes + k; // leaf node
        while index != 0 {
            let offset = (index - 1) & BIT_MASK;
            index = (index - 1) >> BIT_SHIFT; // parent node
            debug_assert!(self.tree[index][offset] >= weight);
            // SAFETY: Index is updated to a lesser value towards zero. The
            // bitwise AND operation with BIT_MASK ensures that offset is
            // always less than FANOUT, which is the size of the inner arrays.
            // As a result, tree[index][offset] never goes out of bounds.
            unsafe { self.tree.get_unchecked_mut(index).get_unchecked_mut(offset) }
                .sub_assign(weight);
        }
    }

    // Returns smallest index such that sum of weights[..=k] > val,
    // along with its respective weight.
    fn search(&self, mut val: T) -> (/*index:*/ usize, /*weight:*/ T) {
        debug_assert!(val >= Self::ZERO);
        debug_assert!(val < self.weight);
        debug_assert!(!self.tree.is_empty());
        // Traverse the tree downwards from the root to the target leaf node.
        let mut index = 0; // root
        loop {
            // SAFETY: function returns if index goes out of bounds.
            let (offset, &node) = unsafe { self.tree.get_unchecked(index) }
                .iter()
                .enumerate()
                .find(|(_, &node)| {
                    if val < node {
                        true
                    } else {
                        val -= node;
                        false
                    }
                })
                .unwrap();
            // Traverse to the subtree of self.tree[index].
            index = (index << BIT_SHIFT) + offset + 1;
            if self.tree.len() <= index {
                return (index - self.num_nodes, node);
            }
        }
    }

    pub fn remove_index(&mut self, k: usize) {
        let index = self.num_nodes + k; // leaf node
        let offset = (index - 1) & BIT_MASK;
        let index = (index - 1) >> BIT_SHIFT; // parent node
        let Some(weight) = self.tree.get(index).map(|node| node[offset]) else {
            error!("WeightedShuffle::remove_index: Invalid index {k}");
            return;
        };
        if weight == Self::ZERO {
            self.remove_zero(k);
        } else {
            self.remove(k, weight);
        }
    }

    fn remove_zero(&mut self, k: usize) {
        if let Some(index) = self.zeros.iter().position(|&ix| ix == k) {
            self.zeros.remove(index);
        }
    }
}

impl<T> WeightedShuffle<T>
where
    T: Copy + ConstZero + PartialOrd + SampleUniform + SubAssign,
{
    // Equivalent to weighted_shuffle.shuffle(&mut rng).next()
    pub fn first<R: Rng>(&self, rng: &mut R) -> Option<usize> {
        if self.weight > Self::ZERO {
            let sample = <T as SampleUniform>::Sampler::sample_single(Self::ZERO, self.weight, rng);
            let (index, _) = self.search(sample);
            return Some(index);
        }
        if self.zeros.is_empty() {
            return None;
        }
        let index = <usize as SampleUniform>::Sampler::sample_single(0usize, self.zeros.len(), rng);
        self.zeros.get(index).copied()
    }
}

impl<T> WeightedShuffle<T>
where
    T: Copy + ConstZero + PartialOrd + SampleUniform + SubAssign,
{
    pub fn shuffle<'a, R: Rng>(&'a mut self, rng: &'a mut R) -> impl Iterator<Item = usize> + 'a {
        std::iter::from_fn(move || {
            if self.weight > Self::ZERO {
                let sample =
                    <T as SampleUniform>::Sampler::sample_single(Self::ZERO, self.weight, rng);
                let (index, weight) = self.search(sample);
                self.remove(index, weight);
                return Some(index);
            }
            if self.zeros.is_empty() {
                return None;
            }
            let index =
                <usize as SampleUniform>::Sampler::sample_single(0usize, self.zeros.len(), rng);
            Some(self.zeros.swap_remove(index))
        })
    }
}

// Maps number of items to the number of "internal" nodes of the tree
// which "implicitly" holds those items on the leaves.
// Nodes without children are never accessed and don't need to be
// allocated, so the tree size is the second smaller number.
fn get_num_nodes_and_tree_size(count: usize) -> (/*num_nodes:*/ usize, /*tree_size:*/ usize) {
    let mut size: usize = 0;
    let mut nodes: usize = 1;
    while nodes * FANOUT < count {
        size += nodes;
        nodes *= FANOUT;
    }
    (size + nodes, size + count.div_ceil(FANOUT))
}

// #[derive(Clone)] does not overwrite clone_from which is used in
// retransmit-stage to minimize allocations.
impl<T: Clone> Clone for WeightedShuffle<T> {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            num_nodes: self.num_nodes,
            tree: self.tree.clone(),
            weight: self.weight.clone(),
            zeros: self.zeros.clone(),
        }
    }

    #[inline]
    fn clone_from(&mut self, other: &Self) {
        self.num_nodes = other.num_nodes;
        self.tree.clone_from(&other.tree);
        self.weight = other.weight.clone();
        self.zeros.clone_from(&other.zeros);
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        itertools::Itertools,
        rand::SeedableRng,
        rand_chacha::ChaChaRng,
        solana_sdk::hash::Hash,
        std::{
            convert::TryInto,
            iter::{repeat_with, successors, Sum},
            str::FromStr,
        },
        test_case::test_case,
    };

    fn verify_shuffle<T>(shuffle: &[usize], weights: &[T], mut mask: Vec<bool>)
    where
        T: ConstZero + Copy + PartialEq + PartialOrd + Sum<T>,
    {
        assert_eq!(weights.len(), mask.len());
        let num_dropped = mask.iter().copied().map(usize::from).sum::<usize>();
        // Assert that only the indices which were not dropped appear in
        // the shuffle.
        assert_eq!(shuffle.len(), weights.len() - num_dropped);
        assert!(shuffle.iter().all(|&index| {
            let out = !mask[index];
            mask[index] = true;
            out
        }));
        assert!(mask.iter().all(|&x| x));
        // Assert that the random shuffle is weighted.
        assert!(shuffle
            .chunks(shuffle.len() / 10)
            .map(|chunk| chunk.iter().map(|&i| weights[i]).sum::<T>())
            .tuple_windows()
            .all(|(a, b)| a > b));
        // Assert that zero weights only appear at the end of the shuffle.
        assert!(shuffle
            .iter()
            .tuple_windows()
            .all(|(&i, &j)| weights[i] != T::ZERO || weights[j] == T::ZERO));
    }

    fn weighted_shuffle_slow<R>(rng: &mut R, mut weights: Vec<u64>) -> Vec<usize>
    where
        R: Rng,
    {
        let mut shuffle = Vec::with_capacity(weights.len());
        let mut high: u64 = weights.iter().sum();
        let mut zeros: Vec<_> = weights
            .iter()
            .enumerate()
            .filter(|(_, w)| **w == 0)
            .map(|(i, _)| i)
            .collect();
        while high != 0 {
            let sample = rng.gen_range(0..high);
            let index = weights
                .iter()
                .scan(0, |acc, &w| {
                    *acc += w;
                    Some(*acc)
                })
                .position(|acc| sample < acc)
                .unwrap();
            shuffle.push(index);
            high -= weights[index];
            weights[index] = 0;
        }
        while !zeros.is_empty() {
            let index = <usize as SampleUniform>::Sampler::sample_single(0usize, zeros.len(), rng);
            shuffle.push(zeros.swap_remove(index));
        }
        shuffle
    }

    #[test]
    fn test_get_num_nodes_and_tree_size() {
        assert_eq!(get_num_nodes_and_tree_size(0), (1, 0));
        for count in 1..=16 {
            assert_eq!(get_num_nodes_and_tree_size(count), (1, 1));
        }
        let num_nodes = 1 + 16;
        for count in 17..=256 {
            let tree_size = 1 + (count + 15) / 16;
            assert_eq!(get_num_nodes_and_tree_size(count), (num_nodes, tree_size));
        }
        let num_nodes = 1 + 16 + 16 * 16;
        for count in 257..=4096 {
            let tree_size = 1 + 16 + (count + 15) / 16;
            assert_eq!(get_num_nodes_and_tree_size(count), (num_nodes, tree_size));
        }
        let num_nodes = 1 + 16 + 16 * 16 + 16 * 16 * 16;
        for count in 4097..=65536 {
            let tree_size = 1 + 16 + 16 * 16 + (count + 15) / 16;
            assert_eq!(get_num_nodes_and_tree_size(count), (num_nodes, tree_size));
        }
    }

    // Asserts that empty weights will return empty shuffle.
    #[test]
    fn test_weighted_shuffle_empty_weights() {
        let weights = Vec::<u64>::new();
        let mut rng = rand::thread_rng();
        let shuffle = WeightedShuffle::new("", weights);
        assert!(shuffle.clone().shuffle(&mut rng).next().is_none());
        assert!(shuffle.first(&mut rng).is_none());
    }

    // Asserts that zero weights will be shuffled.
    #[test]
    fn test_weighted_shuffle_zero_weights() {
        let weights = vec![0u64; 5];
        let seed = [37u8; 32];
        let mut rng = ChaChaRng::from_seed(seed);
        let shuffle = WeightedShuffle::new("", weights);
        assert_eq!(
            shuffle.clone().shuffle(&mut rng).collect::<Vec<_>>(),
            [1, 4, 2, 3, 0]
        );
        let mut rng = ChaChaRng::from_seed(seed);
        assert_eq!(shuffle.first(&mut rng), Some(1));
    }

    // Asserts that each index is selected proportional to its weight.
    #[test]
    fn test_weighted_shuffle_sanity() {
        let seed: Vec<_> = (1..).step_by(3).take(32).collect();
        let seed: [u8; 32] = seed.try_into().unwrap();
        let mut rng = ChaChaRng::from_seed(seed);
        let weights = [1, 0, 1000, 0, 0, 10, 100, 0];
        let mut counts = [0; 8];
        for _ in 0..100000 {
            let mut weighted_shuffle = WeightedShuffle::new("", weights);
            let mut shuffle = weighted_shuffle.shuffle(&mut rng);
            counts[shuffle.next().unwrap()] += 1;
            let _ = shuffle.count(); // consume the rest.
        }
        assert_eq!(counts, [95, 0, 90069, 0, 0, 908, 8928, 0]);
        let mut counts = [0; 8];
        for _ in 0..100000 {
            let mut shuffle = WeightedShuffle::new("", weights);
            shuffle.remove_index(5);
            shuffle.remove_index(3);
            shuffle.remove_index(1);
            let mut shuffle = shuffle.shuffle(&mut rng);
            counts[shuffle.next().unwrap()] += 1;
            let _ = shuffle.count(); // consume the rest.
        }
        assert_eq!(counts, [97, 0, 90862, 0, 0, 0, 9041, 0]);
    }

    #[test]
    fn test_weighted_shuffle_negative_overflow() {
        const SEED: [u8; 32] = [48u8; 32];
        let weights = [19i64, 23, 7, 0, 0, 23, 3, 0, 5, 0, 19, 29];
        let mut rng = ChaChaRng::from_seed(SEED);
        let mut shuffle = WeightedShuffle::new("", weights);
        assert_eq!(
            shuffle.shuffle(&mut rng).collect::<Vec<_>>(),
            [8, 1, 5, 10, 11, 0, 2, 6, 9, 4, 3, 7]
        );
        // Negative weights and overflowing ones are treated as zero.
        let weights = [19, 23, 7, -57, i64::MAX, 23, 3, i64::MAX, 5, -79, 19, 29];
        let mut rng = ChaChaRng::from_seed(SEED);
        let mut shuffle = WeightedShuffle::new("", weights);
        assert_eq!(
            shuffle.shuffle(&mut rng).collect::<Vec<_>>(),
            [8, 1, 5, 10, 11, 0, 2, 6, 9, 4, 3, 7]
        );
    }

    #[test]
    fn test_weighted_shuffle_hard_coded() {
        let weights = [
            78, 70, 38, 27, 21, 0, 82, 42, 21, 77, 77, 0, 17, 4, 50, 96, 0, 83, 33, 16, 72,
        ];
        let seed = [48u8; 32];
        let mut rng = ChaChaRng::from_seed(seed);
        let mut shuffle = WeightedShuffle::new("", weights);
        assert_eq!(
            shuffle.clone().shuffle(&mut rng).collect::<Vec<_>>(),
            [2, 12, 18, 0, 14, 15, 17, 10, 1, 9, 7, 6, 13, 20, 4, 19, 3, 8, 11, 16, 5]
        );
        let mut rng = ChaChaRng::from_seed(seed);
        assert_eq!(shuffle.first(&mut rng), Some(2));
        let mut rng = ChaChaRng::from_seed(seed);
        shuffle.remove_index(11);
        shuffle.remove_index(3);
        shuffle.remove_index(15);
        shuffle.remove_index(0);
        assert_eq!(
            shuffle.clone().shuffle(&mut rng).collect::<Vec<_>>(),
            [4, 6, 1, 12, 19, 14, 17, 20, 2, 9, 10, 8, 7, 18, 13, 5, 16]
        );
        let mut rng = ChaChaRng::from_seed(seed);
        assert_eq!(shuffle.first(&mut rng), Some(4));
        let seed = [37u8; 32];
        let mut rng = ChaChaRng::from_seed(seed);
        let mut shuffle = WeightedShuffle::new("", weights);
        assert_eq!(
            shuffle.clone().shuffle(&mut rng).collect::<Vec<_>>(),
            [19, 3, 15, 14, 6, 10, 17, 18, 9, 2, 4, 1, 0, 7, 8, 20, 12, 13, 16, 5, 11]
        );
        let mut rng = ChaChaRng::from_seed(seed);
        assert_eq!(shuffle.first(&mut rng), Some(19));
        shuffle.remove_index(16);
        shuffle.remove_index(8);
        shuffle.remove_index(20);
        shuffle.remove_index(5);
        shuffle.remove_index(19);
        shuffle.remove_index(4);
        let mut rng = ChaChaRng::from_seed(seed);
        assert_eq!(
            shuffle.clone().shuffle(&mut rng).collect::<Vec<_>>(),
            [17, 2, 9, 14, 6, 10, 12, 1, 15, 13, 7, 0, 18, 3, 11]
        );
        let mut rng = ChaChaRng::from_seed(seed);
        assert_eq!(shuffle.first(&mut rng), Some(17));
    }

    // Verifies that changes to the code or dependencies (e.g. rand or
    // rand_chacha::ChaChaRng) do not change the deterministic shuffle.
    #[test_case(0x587c27258191c66d, "84jN8bvnp6mvtngzt42SW8AtRf5fcv3VBerKkUsYrCVG")]
    #[test_case(0x7dad2afc68808779, "25oFhs9sR3WYfB6ohy752JrbLqpBjw6X4Eszbcsoxon4")]
    #[test_case(0xfdd71c99c936736c, "7H9H8V7ccmpBhC3i5vEeFfiUwvRSAvRWadZhFH5ecSD7")]
    #[test_case(0xe2a4d9fdd186636c, "Nxe6X7f74kEPrJFycKFcxByDRWKJtx1J3vsdbum9VPv")]
    #[test_case(0x19a0a360e9f3094d, "Ec6wiaqDuVc5AzZpq4GAZ6GLsRJvw9mAVWVrCpDoGaRm")]
    #[test_case(0xc5e0204894ca50dc, "BqxDzSFw8rJRHnTZmsPRzF77G3xgfK4hD8JyYeAFfxuZ")]
    #[test_case(0xf1336cf933eeda07, "3Ux2vciDFdgNqULpsQpXfpaxZykWmBFCseqX9dwpGnyH")]
    #[test_case(0xe666e7514f37c7a1, "Fc3gAUgh2mD1se3kkhPnLMKpQCiARd2PSdGf7b2fDS2n")]
    fn test_weighted_shuffle_hard_coded_paranoid(seed: u64, expected_hash: &str) {
        let expected_hash = Hash::from_str(expected_hash).unwrap();
        let mut rng = <[u8; 32]>::try_from(
            successors(Some(seed), |seed| Some(seed + 1))
                .map(u64::to_le_bytes)
                .take(32 / 8)
                .flatten()
                .collect::<Vec<u8>>(),
        )
        .map(ChaChaRng::from_seed)
        .unwrap();
        let num_weights = rng.gen_range(1..=100_000);
        assert!((8143..=85348).contains(&num_weights), "{num_weights}");
        let weights: Vec<u64> = repeat_with(|| {
            if rng.gen_ratio(1, 100) {
                0u64 // 1% zero weights.
            } else {
                rng.gen_range(0..=(u64::MAX / num_weights as u64))
            }
        })
        .take(num_weights)
        .collect();
        let num_zeros = weights.iter().filter(|&&w| w == 0).count();
        assert!((72..=846).contains(&num_zeros), "{num_zeros}");
        // Assert that the sum of weights does not overflow.
        assert_eq!(
            weights.iter().fold(0u64, |a, &b| a.checked_add(b).unwrap()),
            weights.iter().sum::<u64>()
        );
        let mut shuffle = WeightedShuffle::<u64>::new("", &weights);
        let shuffle1 = shuffle.clone().shuffle(&mut rng).collect::<Vec<_>>();
        // Assert that all indices appear in the shuffle.
        assert_eq!(shuffle1.len(), num_weights);
        verify_shuffle(&shuffle1, &weights, vec![false; num_weights]);
        // Drop some of the weights and re-shuffle.
        let num_drops = rng.gen_range(1..1_000);
        assert!((253..=981).contains(&num_drops), "{num_drops}");
        let mut mask = vec![false; num_weights];
        repeat_with(|| rng.gen_range(0..num_weights))
            .filter(|&index| {
                if mask[index] {
                    false
                } else {
                    mask[index] = true;
                    true
                }
            })
            .take(num_drops)
            .for_each(|index| shuffle.remove_index(index));
        let shuffle2 = shuffle.shuffle(&mut rng).collect::<Vec<_>>();
        assert_eq!(shuffle2.len(), num_weights - num_drops);
        verify_shuffle(&shuffle2, &weights, mask);
        // Assert that code or dependencies updates do not change the shuffle.
        let bytes = shuffle1
            .into_iter()
            .chain(shuffle2)
            .map(usize::to_le_bytes)
            .collect::<Vec<_>>();
        let bytes = bytes.iter().map(AsRef::as_ref).collect::<Vec<_>>();
        assert_eq!(solana_sdk::hash::hashv(&bytes[..]), expected_hash);
    }

    #[test]
    fn test_weighted_shuffle_match_slow() {
        let mut rng = rand::thread_rng();
        let weights: Vec<u64> = repeat_with(|| rng.gen_range(0..1000)).take(997).collect();
        for _ in 0..10 {
            let mut seed = [0u8; 32];
            rng.fill(&mut seed[..]);
            let mut rng = ChaChaRng::from_seed(seed);
            let mut shuffle = WeightedShuffle::<u64>::new("", &weights);
            let shuffle: Vec<_> = shuffle.shuffle(&mut rng).collect();
            let mut rng = ChaChaRng::from_seed(seed);
            let shuffle_slow = weighted_shuffle_slow(&mut rng, weights.clone());
            assert_eq!(shuffle, shuffle_slow);
            let mut rng = ChaChaRng::from_seed(seed);
            let shuffle = WeightedShuffle::<u64>::new("", &weights);
            assert_eq!(shuffle.first(&mut rng), Some(shuffle_slow[0]));
        }
    }

    #[test]
    fn test_weighted_shuffle_paranoid() {
        let mut rng = rand::thread_rng();
        for size in 0..1351 {
            let weights: Vec<_> = repeat_with(|| rng.gen_range(0..1000)).take(size).collect();
            let seed = rng.gen::<[u8; 32]>();
            let mut rng = ChaChaRng::from_seed(seed);
            let shuffle_slow = weighted_shuffle_slow(&mut rng.clone(), weights.clone());
            let mut shuffle = WeightedShuffle::new("", weights);
            if size > 0 {
                assert_eq!(shuffle.first(&mut rng.clone()), Some(shuffle_slow[0]));
            }
            assert_eq!(shuffle.shuffle(&mut rng).collect::<Vec<_>>(), shuffle_slow);
        }
    }
}
