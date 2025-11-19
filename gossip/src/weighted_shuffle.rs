//! The `weighted_shuffle` module provides an iterator over shuffled weights.

use {
    num_traits::CheckedAdd,
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
pub struct WeightedShuffle {
    // Number of "internal" nodes of the tree.
    num_nodes: usize,
    // Underlying array implementing the tree.
    // Nodes without children are never accessed and don't need to be
    // allocated, so tree.len() < num_nodes.
    // tree[i][j] is the sum of all weights in the j'th sub-tree of node i.
    tree: Vec<[u64; FANOUT]>,
    // Current sum of all weights, excluding already sampled ones.
    weight: u64,
    // Indices of zero weighted entries.
    zeros: Vec<usize>,
}

impl WeightedShuffle {
    /// If weights overflow the total sum they are treated as zero.
    pub fn new<I>(name: &'static str, weights: I) -> Self
    where
        I: IntoIterator<Item: Borrow<u64>>,
        <I as IntoIterator>::IntoIter: ExactSizeIterator,
    {
        let weights = weights.into_iter();
        let (num_nodes, size) = get_num_nodes_and_tree_size(weights.len());
        debug_assert!(size <= num_nodes);
        let mut tree = vec![[0; FANOUT]; size];
        let mut sum = 0;
        let mut zeros = Vec::default();
        let mut num_overflow: usize = 0;
        for (k, weight) in weights.enumerate() {
            let weight = *weight.borrow();
            if weight == 0 {
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

impl WeightedShuffle {
    // Removes given weight at index k.
    fn remove(&mut self, k: usize, weight: u64) {
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
    fn search(&self, mut val: u64) -> (/*index:*/ usize, /*weight:*/ u64) {
        debug_assert!(!self.tree.is_empty());
        // Traverse the tree downwards from the root to the target leaf node.
        let mut index = 0; // root
        loop {
            // SAFETY: function returns if index goes out of bounds.
            let (offset, &node) = unsafe { self.tree.get_unchecked(index) }
                .iter()
                .enumerate()
                .find(|&(_, node)| {
                    if val < *node {
                        true
                    } else {
                        val -= *node;
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
        if weight == 0 {
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

impl WeightedShuffle {
    // Equivalent to weighted_shuffle.shuffle(&mut rng).next()
    pub fn first<R: Rng>(&self, rng: &mut R) -> Option<usize> {
        if self.weight > 0 {
            let sample = <u64 as SampleUniform>::Sampler::sample_single(0, self.weight, rng);
            let (index, _) = self.search(sample);
            return Some(index);
        }
        if self.zeros.is_empty() {
            return None;
        }
        let index = <u64 as SampleUniform>::Sampler::sample_single(0, self.zeros.len() as u64, rng);
        self.zeros.get(index as usize).copied()
    }
}

impl WeightedShuffle {
    pub fn shuffle<'a, R: Rng>(&'a mut self, rng: &'a mut R) -> impl Iterator<Item = usize> + 'a {
        std::iter::from_fn(move || {
            if self.weight > 0 {
                let sample = <u64 as SampleUniform>::Sampler::sample_single(0, self.weight, rng);
                let (index, weight) = self.search(sample);
                self.remove(index, weight);
                return Some(index);
            }
            if self.zeros.is_empty() {
                return None;
            }
            let index =
                <u64 as SampleUniform>::Sampler::sample_single(0, self.zeros.len() as u64, rng);
            Some(self.zeros.swap_remove(index as usize))
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
impl Clone for WeightedShuffle {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            num_nodes: self.num_nodes,
            tree: self.tree.clone(),
            weight: self.weight,
            zeros: self.zeros.clone(),
        }
    }

    #[inline]
    fn clone_from(&mut self, other: &Self) {
        self.num_nodes = other.num_nodes;
        self.tree.clone_from(&other.tree);
        self.weight = other.weight;
        self.zeros.clone_from(&other.zeros);
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        itertools::Itertools,
        rand::SeedableRng,
        rand_chacha::{ChaCha8Rng, ChaChaRng},
        solana_hash::Hash,
        std::{
            convert::TryInto,
            iter::{repeat_with, successors},
            str::FromStr,
        },
        test_case::test_case,
    };

    fn verify_shuffle(shuffle: &[usize], weights: &[u64], mut mask: Vec<bool>) {
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
            .map(|chunk| chunk.iter().map(|&i| weights[i]).sum::<u64>())
            .tuple_windows()
            .all(|(a, b)| a > b));
        // Assert that zero weights only appear at the end of the shuffle.
        assert!(shuffle
            .iter()
            .tuple_windows()
            .all(|(&i, &j)| weights[i] != 0 || weights[j] == 0));
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
            let index = <u64 as SampleUniform>::Sampler::sample_single(0, zeros.len() as u64, rng);
            shuffle.push(zeros.swap_remove(index as usize));
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
        for count in 17_usize..=256 {
            let tree_size = 1 + count.div_ceil(16);
            assert_eq!(get_num_nodes_and_tree_size(count), (num_nodes, tree_size));
        }
        let num_nodes = 1 + 16 + 16 * 16;
        for count in 257_usize..=4096 {
            let tree_size = 1 + 16 + count.div_ceil(16);
            assert_eq!(get_num_nodes_and_tree_size(count), (num_nodes, tree_size));
        }
        let num_nodes = 1 + 16 + 16 * 16 + 16 * 16 * 16;
        for count in 4097_usize..=65536 {
            let tree_size = 1 + 16 + 16 * 16 + count.div_ceil(16);
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
    #[test_case(8)]
    #[test_case(20)]
    fn test_weighted_shuffle_zero_weights(cha_cha_variant: u8) {
        let weights = vec![0u64; 5];
        let seed = [37u8; 32];
        let shuffle = WeightedShuffle::new("", weights);
        match cha_cha_variant {
            8 => {
                let mut rng = ChaCha8Rng::from_seed(seed);
                assert_eq!(
                    shuffle.clone().shuffle(&mut rng).collect::<Vec<_>>(),
                    [4, 3, 1, 2, 0],
                );
                let mut rng = ChaCha8Rng::from_seed(seed);
                assert_eq!(shuffle.first(&mut rng), Some(4));
            }
            20 => {
                let mut rng = ChaChaRng::from_seed(seed);
                assert_eq!(
                    shuffle.clone().shuffle(&mut rng).collect::<Vec<_>>(),
                    [1, 4, 2, 3, 0],
                );
                let mut rng = ChaChaRng::from_seed(seed);
                assert_eq!(shuffle.first(&mut rng), Some(1));
            }
            _ => unreachable!(),
        };
    }

    // Asserts that each index is selected proportional to its weight.
    #[test]
    fn test_weighted_shuffle_sanity() {
        let seed: Vec<_> = (1..).step_by(3).take(32).collect();
        let seed: [u8; 32] = seed.try_into().unwrap();
        let mut rng = ChaChaRng::from_seed(seed);
        test_weighted_shuffle_sanity_impl(
            &mut rng,
            &[80, 0, 89972, 0, 0, 890, 9058, 0],
            &[94, 0, 90781, 0, 0, 0, 9125, 0],
        );
        let mut rng = ChaCha8Rng::from_seed(seed);
        test_weighted_shuffle_sanity_impl(
            &mut rng,
            &[96, 0, 89962, 0, 0, 891, 9051, 0],
            &[88, 0, 90680, 0, 0, 0, 9232, 0],
        );
        fn test_weighted_shuffle_sanity_impl<R: Rng>(
            rng: &mut R,
            counts1: &[i32],
            counts2: &[i32],
        ) {
            let weights = [1u64, 0, 1000, 0, 0, 10, 100, 0];
            let mut counts = [0; 8];
            for _ in 0..100000 {
                let mut weighted_shuffle = WeightedShuffle::new("", weights);
                let mut shuffle = weighted_shuffle.shuffle(rng);
                counts[shuffle.next().unwrap()] += 1;
                let _ = shuffle.count(); // consume the rest.
            }
            assert_eq!(counts, counts1);
            let mut counts = [0; 8];
            for _ in 0..100000 {
                let mut shuffle = WeightedShuffle::new("", weights);
                shuffle.remove_index(5);
                shuffle.remove_index(3);
                shuffle.remove_index(1);
                let mut shuffle = shuffle.shuffle(rng);
                counts[shuffle.next().unwrap()] += 1;
                let _ = shuffle.count(); // consume the rest.
            }
            assert_eq!(counts, counts2);
        }
    }

    #[test]
    fn test_weighted_shuffle_overflow() {
        test_weighted_shuffle_overflow_impl::<ChaChaRng>(&[8, 1, 5, 10, 11, 0, 2, 6, 9, 4, 3, 7]);
        test_weighted_shuffle_overflow_impl::<ChaCha8Rng>(&[5, 11, 2, 0, 10, 1, 6, 8, 7, 3, 9, 4]);

        fn test_weighted_shuffle_overflow_impl<R: Rng + rand::SeedableRng<Seed = [u8; 32]>>(
            counts: &[usize],
        ) {
            const SEED: [u8; 32] = [48u8; 32];
            let weights = [19u64, 23, 7, 0, 0, 23, 3, 0, 5, 0, 19, 29];
            let mut rng = R::from_seed(SEED);
            let mut shuffle = WeightedShuffle::new("", weights);
            assert_eq!(shuffle.shuffle(&mut rng).collect::<Vec<_>>(), counts);
        }
    }

    #[test]
    fn test_weighted_shuffle_hard_coded() {
        let weights = [
            78u64, 70, 38, 27, 21, 0, 82, 42, 21, 77, 77, 0, 17, 4, 50, 96, 0, 83, 33, 16, 72,
        ];
        let seed = [48u8; 32];
        let mut rng = ChaChaRng::from_seed(seed);
        let mut shuffle = WeightedShuffle::new("", weights);
        assert_eq!(
            shuffle.clone().shuffle(&mut rng).collect::<Vec<_>>(),
            [10, 3, 14, 18, 0, 9, 19, 6, 2, 1, 17, 7, 13, 15, 20, 12, 4, 8, 5, 16, 11]
        );
        let mut rng = ChaChaRng::from_seed(seed);
        assert_eq!(shuffle.first(&mut rng), Some(10));
        let mut rng = ChaChaRng::from_seed(seed);
        shuffle.remove_index(11);
        shuffle.remove_index(3);
        shuffle.remove_index(15);
        shuffle.remove_index(0);
        assert_eq!(
            shuffle.clone().shuffle(&mut rng).collect::<Vec<_>>(),
            [10, 6, 9, 17, 20, 8, 4, 1, 2, 14, 7, 12, 18, 19, 13, 16, 5]
        );
        let mut rng = ChaChaRng::from_seed(seed);
        assert_eq!(shuffle.first(&mut rng), Some(10));
        let seed = [37u8; 32];
        let mut rng = ChaChaRng::from_seed(seed);
        let mut shuffle = WeightedShuffle::new("", weights);
        assert_eq!(
            shuffle.clone().shuffle(&mut rng).collect::<Vec<_>>(),
            [3, 15, 10, 6, 19, 17, 2, 0, 9, 20, 1, 14, 7, 8, 12, 18, 4, 13, 5, 11, 16]
        );
        let mut rng = ChaChaRng::from_seed(seed);
        assert_eq!(shuffle.first(&mut rng), Some(3));
        shuffle.remove_index(16);
        shuffle.remove_index(8);
        shuffle.remove_index(20);
        shuffle.remove_index(5);
        shuffle.remove_index(19);
        shuffle.remove_index(4);
        let mut rng = ChaChaRng::from_seed(seed);
        assert_eq!(
            shuffle.clone().shuffle(&mut rng).collect::<Vec<_>>(),
            [2, 14, 10, 6, 17, 15, 3, 9, 12, 0, 1, 7, 18, 13, 11]
        );
        let mut rng = ChaChaRng::from_seed(seed);
        assert_eq!(shuffle.first(&mut rng), Some(2));
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
        let mut shuffle = WeightedShuffle::new("", &weights);
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
        assert_eq!(solana_sha256_hasher::hashv(&bytes[..]), expected_hash);
    }

    #[test]
    fn test_weighted_shuffle_match_slow() {
        test_weighted_shuffle_match_slow_impl::<ChaChaRng>();
        test_weighted_shuffle_match_slow_impl::<ChaCha8Rng>();

        fn test_weighted_shuffle_match_slow_impl<R: Rng + rand::SeedableRng<Seed = [u8; 32]>>() {
            let mut rng = rand::thread_rng();
            let weights: Vec<u64> = repeat_with(|| rng.gen_range(0..1000)).take(997).collect();
            for _ in 0..10 {
                let mut seed = [0u8; 32];
                rng.fill(&mut seed[..]);
                let mut rng = R::from_seed(seed);
                let mut shuffle = WeightedShuffle::new("", &weights);
                let shuffle: Vec<_> = shuffle.shuffle(&mut rng).collect();
                let mut rng = R::from_seed(seed);
                let shuffle_slow = weighted_shuffle_slow(&mut rng, weights.clone());
                assert_eq!(shuffle, shuffle_slow);
                let mut rng = R::from_seed(seed);
                let shuffle = WeightedShuffle::new("", &weights);
                assert_eq!(shuffle.first(&mut rng), Some(shuffle_slow[0]));
            }
        }
    }

    #[test]
    fn test_weighted_shuffle_paranoid() {
        let mut rng = rand::thread_rng();
        let seed = rng.gen::<[u8; 32]>();
        let rng = ChaCha8Rng::from_seed(seed);
        test_weighted_shuffle_paranoid_impl(rng);
        let rng = ChaChaRng::from_seed(seed);
        test_weighted_shuffle_paranoid_impl(rng);

        fn test_weighted_shuffle_paranoid_impl<R: Rng + Clone>(mut rng: R) {
            for size in 0..1351 {
                let weights: Vec<_> = repeat_with(|| rng.gen_range(0..1000)).take(size).collect();
                let shuffle_slow = weighted_shuffle_slow(&mut rng.clone(), weights.clone());
                let mut shuffle = WeightedShuffle::new("", weights);
                if size > 0 {
                    assert_eq!(shuffle.first(&mut rng.clone()), Some(shuffle_slow[0]));
                }
                assert_eq!(shuffle.shuffle(&mut rng).collect::<Vec<_>>(), shuffle_slow);
            }
        }
    }
}
