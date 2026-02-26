use num_traits::{Float, NumCast};

/// Welford's online algorithm for computing running mean, variance, and standard deviation.
#[derive(Debug, Clone)]
pub struct WelfordStats<F: Float = f64, S: Copy + PartialOrd + NumCast = u64> {
    /// Number of samples added.
    count: u64,
    /// Running mean, updated incrementally with each sample.
    mean: F,
    /// Sum of squared differences from the current mean (used to compute variance).
    m2: F,
    /// Maximum value seen.
    max: Option<S>,
}

impl<F: Float, S: Copy + PartialOrd + NumCast> Default for WelfordStats<F, S> {
    fn default() -> Self {
        Self {
            count: 0,
            mean: F::zero(),
            m2: F::zero(),
            max: None,
        }
    }
}

#[allow(clippy::arithmetic_side_effects)]
impl<F: Float + NumCast, S: Copy + PartialOrd + NumCast> WelfordStats<F, S> {
    /// Adds a sample and updates all running statistics.
    pub fn add_sample(&mut self, value: S) {
        self.count = self.count.checked_add(1).unwrap();
        let v: F = NumCast::from(value).unwrap();
        let count_f: F = NumCast::from(self.count).unwrap();
        let d = v - self.mean;
        self.mean = self.mean + d / count_f;
        self.m2 = self.m2 + d * (v - self.mean);
        self.max = Some(match self.max {
            Some(prev) if prev >= value => prev,
            _ => value,
        });
    }

    /// Returns the number of samples added.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Returns the mean, or `None` if no samples have been added.
    pub fn mean<T: NumCast>(&self) -> Option<T> {
        match self.count {
            0 => None,
            _ => NumCast::from(self.mean),
        }
    }

    /// Returns the sample standard deviation, or `None` if fewer than 2 samples.
    pub fn stddev<T: NumCast>(&self) -> Option<T> {
        match self.count {
            0 | 1 => None,
            n => {
                let n_minus_1: F = NumCast::from(n.saturating_sub(1)).unwrap();
                NumCast::from(self.m2 / n_minus_1)
                    .map(|var: f64| var.sqrt())
                    .and_then(NumCast::from)
            }
        }
    }

    /// Returns the maximum value seen, or `None` if no samples have been added.
    pub fn maximum<T: NumCast>(&self) -> Option<T> {
        self.max.and_then(NumCast::from)
    }

    /// Merges the sufficient statistics of another WelfordStats with this one.
    ///
    /// Uses the parallel variant of Welford's algorithm:
    /// https://en.wikipedia.org/wiki/Algorithms_for_calculating_variance#Parallel_algorithm
    pub fn merge(&mut self, other: Self) {
        if other.count == 0 {
            return;
        }
        if self.count == 0 {
            *self = other;
            return;
        }

        let new_count = self.count.checked_add(other.count).unwrap();
        let new_count_f: F = NumCast::from(new_count).unwrap();
        let self_count_f: F = NumCast::from(self.count).unwrap();
        let other_count_f: F = NumCast::from(other.count).unwrap();

        let delta = other.mean - self.mean;
        self.m2 = self.m2 + other.m2 + delta * delta * self_count_f * other_count_f / new_count_f;

        // Numerically stable combined mean (avoids computing n1*mean1 + n2*mean2 directly).
        self.mean = self.mean + (other_count_f / new_count_f) * (other.mean - self.mean);

        self.max = match (self.max, other.max) {
            (Some(a), Some(b)) if a >= b => Some(a),
            (_, Some(b)) => Some(b),
            (a, None) => a,
        };
        self.count = new_count;
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use {
        super::*,
        rand::{Rng, SeedableRng, rngs::StdRng},
        test_case::test_matrix,
    };

    fn epsilon(f: &str) -> f64 {
        match f {
            "f32" => 1e-4,
            "f64" => 1e-10,
            _ => panic!("unsupported float type: {f}"),
        }
    }

    fn make_stats(values: &[u64]) -> WelfordStats {
        let mut stats = WelfordStats::default();
        values.iter().for_each(|&v| stats.add_sample(v));
        stats
    }

    fn expected_sequential_stddev(n: u64) -> f64 {
        let num = n.saturating_mul(n.saturating_add(1));
        (num as f64 / 12.0).sqrt()
    }

    #[test_matrix([1usize, 5, 10, 100_000])]
    fn test_sequential_stats(n: usize) {
        let stats = make_stats(&(1..=n as u64).collect::<Vec<_>>());

        let expected_mean = (n as f64 + 1.0) / 2.0;
        assert!((stats.mean::<f64>().unwrap() - expected_mean).abs() < epsilon("f64"));
        assert_eq!(stats.maximum::<u64>(), Some(n as u64));

        if n > 1 {
            let expected_stddev = expected_sequential_stddev(n as u64);
            assert!((stats.stddev::<f64>().unwrap() - expected_stddev).abs() < epsilon("f64"));
        }
    }

    #[test_matrix([2usize, 5, 10, 100_000])]
    fn test_constant_has_zero_stddev(n: usize) {
        let stats = make_stats(&vec![999; n]);
        assert_eq!(stats.mean::<i64>(), Some(999));
        assert_eq!(stats.stddev::<f64>(), Some(0.0));
        assert_eq!(stats.maximum::<u64>(), Some(999));
    }

    #[test_matrix(["u64", "u128", "i64", "i128"])]
    fn test_numerical_stability(s: &str) {
        let f = "f64";
        let base: i64 = 1_000_000_000_000;
        let stats = make_test_stats(f, s, &[base, base + 1, base + 2, base + 3, base + 4]);
        assert!((stats.mean().unwrap() - (base + 2) as f64).abs() < epsilon(f));
        assert!((stats.stddev().unwrap() - expected_sequential_stddev(5)).abs() < epsilon(f));

        let base: i64 = 1 << 40;
        let values: Vec<i64> = (0..1000).map(|i| base + i).collect();
        let stats = make_test_stats(f, s, &values);
        assert!((stats.mean().unwrap() - (base as f64 + 499.5)).abs() < epsilon(f));
        assert!(
            (stats.stddev().unwrap() - expected_sequential_stddev(1000)).abs()
                / expected_sequential_stddev(1000)
                < 1e-6
        );
    }

    #[test_matrix(["f32", "f64"], ["u32", "u64", "u128"])]
    fn test_welford_vs_two_pass(f: &str, s: &str) {
        let seed = rand::random::<u64>();
        let mut rng = StdRng::seed_from_u64(seed);
        let data: Vec<i64> = (0..10_000)
            .map(|_| rng.random_range(0..1_000_000i64))
            .collect();
        let stats = make_test_stats(f, s, &data);

        let naive_mean = data.iter().map(|&v| v as f64).sum::<f64>() / data.len() as f64;
        let naive_var = data
            .iter()
            .map(|&v| (v as f64 - naive_mean).powi(2))
            .sum::<f64>()
            / (data.len() - 1) as f64;

        assert!(
            (stats.mean().unwrap() - naive_mean).abs() / naive_mean < epsilon(f),
            "seed={seed}"
        );
        assert!(
            (stats.stddev().unwrap() - naive_var.sqrt()).abs() / naive_var.sqrt() < epsilon(f),
            "seed={seed}"
        );
        assert_eq!(stats.maximum().unwrap(), *data.iter().max().unwrap() as f64);
    }

    #[test_matrix(["u32", "u64", "u128"])]
    fn test_merging_many_chunks(s: &str) {
        let f = "f64";
        let seed = rand::random::<u64>();
        let mut rng = StdRng::seed_from_u64(seed);
        let chunk_sizes = [17, 233, 1, 500, 49];

        let chunks: Vec<Vec<i64>> = chunk_sizes
            .iter()
            .map(|&size| {
                (0..size)
                    .map(|_| rng.random_range(0..1_000_000_000i64))
                    .collect()
            })
            .collect();
        let whole = make_test_stats(f, s, &chunks.iter().flatten().copied().collect::<Vec<_>>());

        let mut merged = make_test_stats(f, s, &chunks[0]);
        for chunk in &chunks[1..] {
            merged.merge(make_test_stats(f, s, chunk));
        }

        assert_eq!(merged.count(), whole.count());
        assert_eq!(merged.maximum(), whole.maximum());
        assert!(
            (merged.mean().unwrap() - whole.mean().unwrap()).abs()
                / whole.mean().unwrap().abs().max(1.0)
                < epsilon(f),
            "seed={seed}"
        );
        assert!(
            (merged.stddev().unwrap() - whole.stddev().unwrap()).abs()
                / whole.stddev().unwrap().abs().max(1.0)
                < epsilon(f),
            "seed={seed}"
        );
    }

    macro_rules! test_stats {
        ($($V:ident($f:ty, $s:ty, $fs:expr, $ss:expr)),+ $(,)?) => {
            enum TestStats { $($V(WelfordStats<$f, $s>)),+ }
            impl TestStats {
                fn new(f: &str, s: &str) -> Self {
                    match (f, s) {
                        $(($fs, $ss) => Self::$V(WelfordStats::default()),)+
                        _ => panic!("unsupported type combo: ({f}, {s})"),
                    }
                }
                fn add(&mut self, v: i64) {
                    match self { $(Self::$V(w) => w.add_sample(NumCast::from(v).unwrap())),+ }
                }
                fn count(&self) -> u64 {
                    match self { $(Self::$V(w) => w.count()),+ }
                }
                fn mean(&self) -> Option<f64> {
                    match self { $(Self::$V(w) => w.mean()),+ }
                }
                fn stddev(&self) -> Option<f64> {
                    match self { $(Self::$V(w) => w.stddev()),+ }
                }
                fn maximum(&self) -> Option<f64> {
                    match self { $(Self::$V(w) => w.maximum()),+ }
                }
                fn merge(&mut self, other: Self) {
                    match (self, other) {
                        $((Self::$V(a), Self::$V(b)) => a.merge(b),)+
                        _ => panic!("type mismatch in merge"),
                    }
                }
            }
        };
    }

    test_stats! {
        F64U8  (f64, u8,   "f64", "u8"),
        F64U16 (f64, u16,  "f64", "u16"),
        F64U32 (f64, u32,  "f64", "u32"),
        F64U64 (f64, u64,  "f64", "u64"),
        F64U128(f64, u128, "f64", "u128"),
        F64I8  (f64, i8,   "f64", "i8"),
        F64I16 (f64, i16,  "f64", "i16"),
        F64I32 (f64, i32,  "f64", "i32"),
        F64I64 (f64, i64,  "f64", "i64"),
        F64I128(f64, i128, "f64", "i128"),
        F32U8  (f32, u8,   "f32", "u8"),
        F32U16 (f32, u16,  "f32", "u16"),
        F32U32 (f32, u32,  "f32", "u32"),
        F32U64 (f32, u64,  "f32", "u64"),
        F32U128(f32, u128, "f32", "u128"),
        F32I8  (f32, i8,   "f32", "i8"),
        F32I16 (f32, i16,  "f32", "i16"),
        F32I32 (f32, i32,  "f32", "i32"),
        F32I64 (f32, i64,  "f32", "i64"),
        F32I128(f32, i128, "f32", "i128"),
    }

    fn make_test_stats(f: &str, s: &str, values: &[i64]) -> TestStats {
        let mut stats = TestStats::new(f, s);
        for &v in values {
            stats.add(v);
        }
        stats
    }

    #[test_matrix(["f32", "f64"], ["u8", "u16", "u32", "u64", "u128"])]
    fn test_type_empty(f: &str, s: &str) {
        let stats = TestStats::new(f, s);
        assert_eq!(stats.count(), 0);
        assert_eq!(stats.mean(), None);
        assert_eq!(stats.stddev(), None);
        assert!(stats.maximum().is_none());
    }

    #[test_matrix(["f32", "f64"], ["u8", "u16", "u32", "u64", "u128"])]
    fn test_type_known_values(f: &str, s: &str) {
        let eps = epsilon(f);
        let stats = make_test_stats(f, s, &[2, 4, 4, 4, 5, 5, 7, 9]);
        assert_eq!(stats.count(), 8);
        assert_eq!(stats.mean(), Some(5.0));
        let sample_var: f64 = 4.0 * 8.0 / 7.0;
        assert!((stats.stddev().unwrap() - sample_var.sqrt()).abs() < eps);
        assert_eq!(stats.maximum(), Some(9.0));
    }

    #[test_matrix(["f32", "f64"], ["u8", "u16", "u32", "u64", "u128"])]
    fn test_type_constant(f: &str, s: &str) {
        let stats = make_test_stats(f, s, &[42; 100]);
        assert_eq!(stats.mean(), Some(42.0));
        assert_eq!(stats.stddev(), Some(0.0));
        assert_eq!(stats.maximum(), Some(42.0));
    }

    #[test_matrix(["f32", "f64"], ["u8", "u16", "u32", "u64", "u128"])]
    fn test_type_sequential(f: &str, s: &str) {
        let eps = epsilon(f);
        let values: Vec<i64> = (1..=50).collect();
        let stats = make_test_stats(f, s, &values);
        assert!((stats.mean().unwrap() - 25.5).abs() < eps);
        assert_eq!(stats.maximum(), Some(50.0));
    }

    #[test_matrix(["f32", "f64"], ["u8", "u16", "u32", "u64", "u128"])]
    fn test_type_merge_empty(f: &str, s: &str) {
        let mut a = TestStats::new(f, s);
        a.merge(TestStats::new(f, s));
        assert_eq!(a.count(), 0);
        a.add(42);
        assert_eq!(a.mean(), Some(42.0));

        // nonempty + empty preserves data
        let mut b = make_test_stats(f, s, &[10, 20, 30]);
        let expected = b.mean().unwrap();
        b.merge(TestStats::new(f, s));
        assert_eq!(b.mean().unwrap(), expected);

        // empty + nonempty copies data
        let mut c = TestStats::new(f, s);
        c.merge(make_test_stats(f, s, &[10, 20, 30]));
        assert_eq!(c.mean().unwrap(), expected);
    }

    #[test_matrix(["f32", "f64"], ["i8", "i16", "i32", "i64", "i128"])]
    fn test_type_signed(f: &str, s: &str) {
        let eps = epsilon(f);
        let stats = make_test_stats(f, s, &[-50, -25, 0, 25, 50]);
        assert!((stats.mean().unwrap() - 0.0).abs() < eps);
        assert_eq!(stats.maximum(), Some(50.0));

        // merge across sign boundary
        let mut neg = make_test_stats(f, s, &[-50, -25, 0]);
        neg.merge(make_test_stats(f, s, &[25, 50]));
        assert!((neg.mean().unwrap() - 0.0).abs() < eps);
    }

    #[test_matrix(["f32", "f64"], ["u8", "u16", "u32", "u64", "u128"])]
    fn test_type_many_samples(f: &str, s: &str) {
        let mut stats = TestStats::new(f, s);
        for i in 0..10_000i64 {
            stats.add(i % 100 + 1);
        }
        let rel_err = (stats.mean().unwrap() - 50.5).abs() / 50.5;
        assert!(rel_err < epsilon(f));
    }

    #[test_matrix(["f32", "f64"], ["u8", "u16", "u32", "u64", "u128"])]
    fn test_type_merge(f: &str, s: &str) {
        let eps = epsilon(f);
        let a: Vec<i64> = (1..=30).collect();
        let b: Vec<i64> = (31..=60).collect();
        let c: Vec<i64> = (61..=90).collect();
        let whole = make_test_stats(f, s, &(1..=90).collect::<Vec<i64>>());

        // commutativity: a+b == b+a
        let mut ab = make_test_stats(f, s, &a);
        ab.merge(make_test_stats(f, s, &b));
        let mut ba = make_test_stats(f, s, &b);
        ba.merge(make_test_stats(f, s, &a));
        assert!((ab.mean().unwrap() - ba.mean().unwrap()).abs() < eps);
        assert!((ab.stddev().unwrap() - ba.stddev().unwrap()).abs() < eps);

        // associativity: (a+b)+c == a+(b+c), and both match the whole
        ab.merge(make_test_stats(f, s, &c));
        let mut bc = make_test_stats(f, s, &b);
        bc.merge(make_test_stats(f, s, &c));
        let mut right = make_test_stats(f, s, &a);
        right.merge(bc);
        assert!((ab.mean().unwrap() - right.mean().unwrap()).abs() < eps);
        assert!((ab.stddev().unwrap() - right.stddev().unwrap()).abs() < eps);
        assert!((ab.mean().unwrap() - whole.mean().unwrap()).abs() < eps);
        assert!((ab.stddev().unwrap() - whole.stddev().unwrap()).abs() < eps);
    }

    #[test_matrix(["f32", "f64"], ["u8", "u16", "u32", "u64", "u128"])]
    fn test_type_merge_asymmetric(f: &str, s: &str) {
        let eps = epsilon(f);
        let big: Vec<i64> = (0..1000).map(|i| i % 100 + 1).collect();
        let small: Vec<i64> = vec![50];
        let whole = make_test_stats(f, s, &[&big[..], &small[..]].concat());

        for (first, second) in [(&big[..], &small[..]), (&small[..], &big[..])] {
            let mut merged = make_test_stats(f, s, first);
            merged.merge(make_test_stats(f, s, second));
            assert!((merged.mean().unwrap() - whole.mean().unwrap()).abs() < eps);
            assert!((merged.stddev().unwrap() - whole.stddev().unwrap()).abs() < eps);
        }
    }

    #[test_matrix(["f32", "f64"], ["u8", "u16", "u32", "u64", "u128"])]
    fn test_type_stddev_n2(f: &str, s: &str) {
        let eps = epsilon(f);
        let s1 = make_test_stats(f, s, &[0, 10]);
        assert!((s1.stddev().unwrap() - 50_f64.sqrt()).abs() < eps);

        assert_eq!(make_test_stats(f, s, &[7, 7]).stddev(), Some(0.0));

        let s2 = make_test_stats(f, s, &[1, 100]);
        let expected = (2.0 * 49.5_f64.powi(2)).sqrt();
        assert!((s2.stddev().unwrap() - expected).abs() / expected < eps);
    }
}
