use num_traits::NumCast;

/// Welford's online algorithm for computing running mean, variance, and standard deviation.
#[derive(Debug, Clone, Default)]
pub struct WelfordStats {
    /// Number of samples added.
    count: u64,
    /// Running mean, updated incrementally with each sample.
    mean: f64,
    /// Sum of squared differences from the current mean (used to compute variance).
    m2: f64,
    /// Maximum value seen.
    max: u64,
}

impl WelfordStats {
    /// Adds a sample and updates all running statistics.
    pub fn add_sample(&mut self, value: u64) {
        self.count = self.count.checked_add(1).unwrap();
        let v = value as f64;
        let d = v - self.mean;
        self.mean += d / self.count as f64;
        self.m2 += d * (v - self.mean);
        self.max = self.max.max(value);
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
                let var = self.m2 / n.saturating_sub(1) as f64;
                NumCast::from(var.sqrt())
            }
        }
    }

    /// Returns the maximum value seen, or `None` if no samples have been added.
    pub fn maximum<T: NumCast>(&self) -> Option<T> {
        match self.count {
            0 => None,
            _ => NumCast::from(self.max),
        }
    }

    /// Merges two sets of stats together.
    pub fn merge(&mut self, other: Self) {
        if other.count == 0 {
            return;
        }
        if self.count == 0 {
            *self = other;
            return;
        }

        // Merge variances together using
        // https://en.wikipedia.org/wiki/Algorithms_for_calculating_variance#Parallel_algorithm
        let new_count = self.count.checked_add(other.count).unwrap();
        let delta = other.mean - self.mean;
        let sum_sq_diff = self.m2
            + other.m2
            + (delta * delta) * self.count as f64 * other.count as f64 / new_count as f64;
        self.m2 = sum_sq_diff;

        // A more stable version of computing the mean.  A less stable but easier to understand
        // formula would be: new_mean = (n1*mean1 + n2*mean2) / (n1 + n2)
        self.mean = self.mean + (other.count as f64 / new_count as f64) * (other.mean - self.mean);

        self.max = self.max.max(other.max);
        self.count = new_count;
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        rand::{rngs::StdRng, Rng, SeedableRng},
        test_case::test_matrix,
    };

    const EPSILON: f64 = 1e-10;

    fn make_stats(values: &[u64]) -> WelfordStats {
        let mut stats = WelfordStats::default();
        values.iter().for_each(|&v| stats.add_sample(v));
        stats
    }

    fn expected_sequential_stddev(n: u64) -> f64 {
        let num = n.saturating_mul(n.saturating_add(1));
        (num as f64 / 12.0).sqrt()
    }

    #[test]
    fn test_empty_returns_none() {
        let stats = WelfordStats::default();
        assert_eq!(stats.count(), 0);
        assert_eq!(stats.mean::<f64>(), None);
        assert_eq!(stats.stddev::<f64>(), None);
        assert_eq!(stats.maximum::<u64>(), None);
    }

    #[test_matrix(
        [1usize, 5, 10, 100_000],
        [false, true]
    )]
    fn test_sample_counts(n: usize, use_sequential: bool) {
        let values: Vec<u64> = if use_sequential {
            (1..=n as u64).collect()
        } else {
            std::iter::repeat_n(42, n).collect()
        };
        let stats = make_stats(&values);

        assert_eq!(stats.count(), n as u64);
        assert!(stats.mean::<f64>().is_some());
        assert!(stats.maximum::<u64>().is_some());
        assert_eq!(stats.stddev::<f64>().is_some(), n > 1);
    }

    #[test_matrix([1usize, 5, 10, 100_000])]
    fn test_sequential_stats(n: usize) {
        let stats = make_stats(&(1..=n as u64).collect::<Vec<_>>());

        let expected_mean = (n as f64 + 1.0) / 2.0;
        assert!((stats.mean::<f64>().unwrap() - expected_mean).abs() < EPSILON);
        assert_eq!(stats.maximum::<u64>(), Some(n as u64));

        if n > 1 {
            let expected_stddev = expected_sequential_stddev(n as u64);
            assert!((stats.stddev::<f64>().unwrap() - expected_stddev).abs() < EPSILON);
        }
    }

    #[test_matrix([2usize, 5, 10, 100_000])]
    fn test_constant_has_zero_stddev(n: usize) {
        let stats = make_stats(&vec![999; n]);
        assert_eq!(stats.mean::<i64>(), Some(999));
        assert_eq!(stats.stddev::<f64>(), Some(0.0));
        assert_eq!(stats.maximum::<u64>(), Some(999));
    }

    #[test]
    fn test_numerical_stability_large_values() {
        let base = 1_000_000_000_000u64;
        let stats = make_stats(&[base, base + 1, base + 2, base + 3, base + 4]);

        assert_eq!(stats.mean::<i64>(), Some((base + 2) as i64));
        assert!((stats.stddev::<f64>().unwrap() - expected_sequential_stddev(5)).abs() < EPSILON);
        assert_eq!(stats.maximum::<u64>(), Some(base + 4));
    }

    #[test]
    fn test_merging() {
        let seed = rand::random::<u64>();
        let mut rng = StdRng::seed_from_u64(seed);
        let first_data = (0..1000).map(|_| rng.random()).collect::<Vec<_>>();

        let mut total = WelfordStats::default();
        let mut first = WelfordStats::default();
        for d in first_data {
            first.add_sample(d);
            total.add_sample(d);
        }
        let second_data = (0..1000).map(|_| rng.random()).collect::<Vec<_>>();
        let mut second = WelfordStats::default();
        for d in second_data {
            second.add_sample(d);
            total.add_sample(d);
        }
        first.merge(second);
        let total_mean = total.mean::<f64>().unwrap();
        let first_mean = first.mean::<f64>().unwrap();
        let diff = (total_mean - first_mean).abs();
        assert!(
            diff / first_mean < EPSILON,
            "seed={seed}, total_mean={total_mean}, first_mean={first_mean}, diff={diff}"
        );
        let total_stddev = total.stddev::<f64>().unwrap();
        let first_stddev = first.stddev::<f64>().unwrap();
        let diff = (total_stddev - first_stddev).abs();
        assert!(
            diff / first_stddev < EPSILON,
            "seed={seed}, total_stddev={total_stddev}, first_stddev={first_stddev}, diff={diff}"
        );
        assert_eq!(total.count(), first.count());
        assert_eq!(total.maximum::<u64>(), first.maximum::<u64>());
    }

    #[test]
    fn test_merging_empty() {
        let mut a = WelfordStats::default();
        a.merge(WelfordStats::default());
        assert_eq!(a.count(), 0);
        assert_eq!(a.mean::<f64>(), None);

        // should not be corrupted by the empty merge
        a.add_sample(42);
        assert_eq!(a.mean::<u64>(), Some(42));

        let mut b = make_stats(&[10, 20, 30]);
        let expected = b.mean::<f64>().unwrap();
        b.merge(WelfordStats::default());
        assert_eq!(b.count(), 3);
        assert_eq!(b.mean::<f64>().unwrap(), expected);
    }
}
