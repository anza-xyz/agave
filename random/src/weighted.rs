use {
    crate::range::UniformU64Sampler,
    rand0_8_5::{distributions::weighted::WeightedError, Rng},
    std::num::NonZero,
};

/// Compatibility weighted sampler for `u64` numbers
///
/// Sampler uses provided `rand::Rng` reference to generate random `u64` numbers and
/// map them to the index of the weight from weights vector provided on initialization.
///
/// This utility exists only to provide compatibility of sampling algorithm with `rand`
/// library at versions <=0.8.5, since parts of the system rely on reproducible sequence
/// of numbers given stable seeded random number generator.
///
/// The algorithm reproduces indices returned by `rand::distributions::WeightedIndex`
/// for the same weights vector and seeded random number generator.
#[derive(Debug)]
pub struct WeightedU64Index {
    weights: Vec<u64>,
    total_weight_sampler: UniformU64Sampler,
}

impl WeightedU64Index {
    pub fn new(mut weights: Vec<u64>) -> Result<Self, WeightedError> {
        // Calculate prefix sum of weights such that binary search can find the index of the
        // chosen weight.
        let mut total_weight = 0u64;
        for weight in weights.iter_mut() {
            total_weight = total_weight.saturating_add(*weight);
            *weight = total_weight;
        }
        if weights.pop().is_none() {
            return Err(WeightedError::NoItem);
        }
        let Some(total_weight) = NonZero::new(total_weight) else {
            return Err(WeightedError::AllWeightsZero);
        };

        Ok(Self {
            weights,
            total_weight_sampler: UniformU64Sampler::new_like_instance_sample(total_weight),
        })
    }

    pub fn sample(&self, rng: &mut impl Rng) -> usize {
        let chosen_weight = self.total_weight_sampler.sample(rng);
        // Find the first item which has a weight *higher* than the chosen weight.
        self.weights.partition_point(|w| *w <= chosen_weight)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        assert_matches::assert_matches,
        rand0_8_5::{distributions::WeightedIndex, prelude::Distribution, SeedableRng as _},
        rand_chacha0_3_1::ChaChaRng,
        sha2::{Digest, Sha256},
        std::array,
        test_case::test_case,
    };

    const CHACHA_SEED: [u8; 32] = [16; 32];

    #[test_case(100, 0, [95, 2, 28, 92, 17, 64, 2, 78, 75, 42])]
    #[test_case(1_000, 0, [952, 25, 285, 925, 176, 648, 29, 781, 759, 427])]
    #[test_case(1_000, 1, [975, 160, 534, 962, 420, 805, 172, 884, 871, 654])]
    #[test_case(1_000, 2, [983, 294, 658, 974, 561, 865, 309, 921, 912, 753])]
    #[test_case(10_000, 1, [9757, 1596, 5346, 9621, 4207, 8052, 1716, 8842, 8712, 6540])]
    fn test_weighted_u64_index_example(num_weights: u64, pow: u32, expected_indices: [usize; 10]) {
        let weights: Vec<_> = (0..num_weights).map(|i| i.pow(pow)).collect();

        let mut rng_compat = ChaChaRng::from_seed(CHACHA_SEED);
        let index_compat =
            WeightedU64Index::new(weights.clone()).expect("non empty and non zero is ok");

        let indices = array::from_fn(|_| index_compat.sample(&mut rng_compat));
        assert_eq!(indices, expected_indices);
    }

    #[test_case(30_000, 0, 50_000, "23K23NXJpui3d9nrKLNfvwpHHRs4dxFfZ8saxH6ZPJyw")]
    #[test_case(20_000, 1, 45_000, "9yvuyu8JDQtUo7cvWJKkc3cUmzWw5RzBJoEtoVR2N2r2")]
    #[test_case(10_000, 2, 35_000, "842FcJe1kmnmrZXAA3rETtBakk1jFdz1dnzMbkf875gh")]
    #[test_case(10_000, 3, 30_000, "5LNbaEBQrb5CzsoHdK79XNDENAJ9WJqW9LpWktqkRchf")]
    fn test_weighted_u64_index_compat(num_weights: u64, pow: u32, len: usize, expected_hash: &str) {
        let weights: Vec<_> = (0..num_weights).map(|i| i.pow(pow)).collect();

        let mut rng_rand = ChaChaRng::from_seed(CHACHA_SEED);
        let index_rand = WeightedIndex::new(weights.clone()).expect("non empty and non zero is ok");

        let mut rng_compat = ChaChaRng::from_seed(CHACHA_SEED);
        let index_compat = WeightedU64Index::new(weights).expect("non empty and non zero is ok");

        let mut hash = Sha256::new();
        (0..len).for_each(|i| {
            let rand = index_rand.sample(&mut rng_rand);
            let compat = index_compat.sample(&mut rng_compat);
            assert_eq!(rand, compat, "should be equal at {i}");
            hash.update(compat.to_le_bytes());
        });
        assert_eq!(&bs58::encode(hash.finalize()).into_string(), expected_hash);
    }

    #[test]
    fn test_weighted_u64_index_error_on_new() {
        assert_matches!(WeightedU64Index::new(vec![]), Err(WeightedError::NoItem));
        assert_matches!(
            WeightedU64Index::new(vec![0, 0, 0, 0, 0]),
            Err(WeightedError::AllWeightsZero)
        );
    }
}
