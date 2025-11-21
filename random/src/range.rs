use {rand0_8_5::Rng, std::num::NonZero};

/// Compatibility uniform range `[0, limit)` sampler for `u64` numbers
///
/// Sampler uses provided `rand::Rng` reference to generate random `u64` numbers and
/// map them to desired range maintaining uniform distribution of generated numbers.
///
/// This utility exists only to provide compatibility of sampling algorithm with `rand`
/// library at versions <=0.8.5, since parts of the system rely on reproducible sequence
/// of numbers given stable seeded random number generator.
///
/// Two sampling algorithms are supported (they initialize internal `zone` value in different ways)
/// to provide compatibility with two ways `rand` sampling can be performed:
/// - `new_like_instance_sample`: reproduces values obtained from sampler instance created with
///   `rand::distributions::uniform::UniformSampler::new` and then used by calling `sample`
/// - `new_like_trait_sample`: reproduces values obtained from trait function calls
///   `rand::distributions::uniform::UniformSampler::sample_single`
#[derive(Debug)]
pub struct UniformU64Sampler {
    range_end: NonZero<u64>,
    zone: u64,
}

impl UniformU64Sampler {
    /// Create sampler reproducing `sample` calls on `UniformInt` instance
    ///
    /// The `zone` internal threshold is obtained by calculating modulo of `u64::MAX`'s difference
    /// with provided range's end to itself. See:
    /// https://github.com/rust-random/rand/blob/937320cbfeebd4352a23086d9c6e68f067f74644/src/distributions/uniform.rs#L458-L504
    #[allow(clippy::arithmetic_side_effects)]
    pub fn new_like_instance_sample(range_end: NonZero<u64>) -> Self {
        let ints_to_reject = (u64::MAX - range_end.get() + 1) % range_end.get();
        let zone = u64::MAX - ints_to_reject;
        Self { range_end, zone }
    }

    /// Create sampler reproducing direct `sample_single` calls on `UniformInt` trait
    ///
    /// The `zone` internal threshold is obtained by calculating 2^{number of leading zeros in provided range}. See
    /// https://github.com/rust-random/rand/blob/937320cbfeebd4352a23086d9c6e68f067f74644/src/distributions/uniform.rs#L534-L553
    pub fn new_like_trait_sample(range_end: NonZero<u64>) -> Self {
        let zone = (range_end.get() << range_end.leading_zeros()).wrapping_sub(1);
        Self { range_end, zone }
    }

    /// Obtain random number from `rng` and map it to the initialized range of this sampler
    pub fn sample(&self, rng: &mut impl Rng) -> u64 {
        loop {
            let (hi, lo) = Self::wmul(rng.r#gen(), self.range_end);
            if lo <= self.zone {
                return hi;
            }
        }
    }

    #[allow(clippy::arithmetic_side_effects)]
    fn wmul(x: u64, y: NonZero<u64>) -> (u64, u64) {
        let tmp = (x as u128) * (y.get() as u128);
        ((tmp >> 64) as u64, tmp as u64)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        rand0_8_5::{
            distributions::uniform::{SampleUniform, UniformSampler as _},
            SeedableRng as _,
        },
        rand_chacha0_3_1::ChaChaRng,
        sha2::{Digest, Sha256},
        std::array,
        test_case::test_case,
    };

    const CHACHA_SEED: [u8; 32] = [16; 32];

    #[test]
    fn test_uniform_sample_like_instance_sample_example() {
        let mut rng_compat = ChaChaRng::from_seed(CHACHA_SEED);
        let sampler_compat =
            UniformU64Sampler::new_like_instance_sample(NonZero::new(294_533).unwrap());
        let values: [u64; 10] = array::from_fn(|_| sampler_compat.sample(&mut rng_compat));
        assert_eq!(
            values,
            [280405, 7507, 84194, 272634, 52124, 190984, 8676, 230277, 223574, 126007]
        );
    }

    #[test_case(10, "5p3DrG89DLrJotbMuZJUDHMrqcfoQiDQpa3FbNDbnND6")]
    #[test_case(2_729, "J65XfpzkppuxWEyvgVdDmGBsJSex6BuevYuDSRtaDNAK")]
    #[test_case(4_098, "EaRABrcPBw5LLNBmjn5ay5YN5yTPgv3GnQJLbpUarRkJ")]
    #[test_case(504_302_479, "HniJPVe7zir8XxHmtUuxzhPVvWjMcJxVhhj8QSs1PZTC")]
    #[test_case(1_000_346_000_000, "BghMy7yLe6BVzMbc4zNvAB4sz3ZSPYKJS5oLGvXYVzw2")]
    fn test_uniform_sampler_like_instance_sample_compat(range_end: u64, expected_hash: &str) {
        let mut rng_rand = ChaChaRng::from_seed(CHACHA_SEED);
        let sampler_rand = <u64 as SampleUniform>::Sampler::new(0, range_end);

        let mut rng_compat = ChaChaRng::from_seed(CHACHA_SEED);
        let sampler_compat =
            UniformU64Sampler::new_like_instance_sample(NonZero::new(range_end).unwrap());

        let mut hash = Sha256::new();
        (0..600_000).for_each(|i| {
            let rand = sampler_rand.sample(&mut rng_rand);
            let compat = sampler_compat.sample(&mut rng_compat);
            assert_eq!(rand, compat, "should be equal at {i}");
            hash.update(compat.to_le_bytes());
        });
        assert_eq!(bs58::encode(hash.finalize()).into_string(), expected_hash);
    }

    #[test]
    fn test_uniform_sample_like_trait_sample_example() {
        let mut rng_compat = ChaChaRng::from_seed(CHACHA_SEED);
        let sampler_compat =
            UniformU64Sampler::new_like_trait_sample(NonZero::new(294_533).unwrap());
        let values: [u64; 10] = array::from_fn(|_| sampler_compat.sample(&mut rng_compat));
        assert_eq!(
            values,
            [272634, 52124, 8676, 230277, 223574, 137788, 212533, 213080, 187008, 209168]
        );
    }

    #[test_case(10, "2tsA6RuXekKvMMvAqjkMMipjYySpw8mrrwwoBeoKohD1")]
    #[test_case(1_000, "58SG5gnD5wR6ngrjxuTv7m1SEXiXejZvuJPUCsyrmqWx")]
    #[test_case(4098, "7w4TY1oaeEeqHmPw3iobD8WVq1BsL5eo7ZuNdvDkoSQo")]
    #[test_case(10_000_000_000, "ENe5A82Wq2nYsH17q9WkKAudXdLE9FVGRbySuuCpaR5k")]
    fn test_uniform_sampler_like_trait_sample_single(range_end: u64, expected_hash: &str) {
        let mut rng_rand = ChaChaRng::from_seed(CHACHA_SEED);

        let mut rng_compat = ChaChaRng::from_seed(CHACHA_SEED);
        let sampler_compat =
            UniformU64Sampler::new_like_trait_sample(NonZero::new(range_end).unwrap());

        let mut hash = Sha256::new();
        (0..1_000).for_each(|i| {
            let rand = <u64 as SampleUniform>::Sampler::sample_single(0, range_end, &mut rng_rand);
            let compat = sampler_compat.sample(&mut rng_compat);
            assert_eq!(rand, compat, "should be equal at {i}");
            hash.update(compat.to_le_bytes());
        });
        assert_eq!(bs58::encode(hash.finalize()).into_string(), expected_hash);
    }
}
