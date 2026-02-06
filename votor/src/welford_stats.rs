use num_traits::NumCast;

/// Welford's online algorithm for computing running mean, variance, and standard deviation.
#[derive(Debug, Clone, Default)]
pub(crate) struct WelfordStats {
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
    pub(crate) fn add_sample(&mut self, value: u64) {
        self.count = self.count.checked_add(1).unwrap();
        let v = value as f64;
        let d = v - self.mean;
        self.mean += d / self.count as f64;
        self.m2 += d * (v - self.mean);
        self.max = self.max.max(value);
    }

    /// Returns the number of samples added.
    pub(crate) fn count(&self) -> u64 {
        self.count
    }

    /// Returns the mean, or `None` if no samples have been added.
    pub(crate) fn mean<T: NumCast>(&self) -> Option<T> {
        match self.count {
            0 => None,
            _ => NumCast::from(self.mean),
        }
    }

    /// Returns the sample standard deviation, or `None` if fewer than 2 samples.
    pub(crate) fn stddev<T: NumCast>(&self) -> Option<T> {
        match self.count {
            0 | 1 => None,
            n => {
                let var = self.m2 / n.saturating_sub(1) as f64;
                NumCast::from(var.sqrt())
            }
        }
    }

    /// Returns the maximum value seen, or `None` if no samples have been added.
    pub(crate) fn maximum<T: NumCast>(&self) -> Option<T> {
        match self.count {
            0 => None,
            _ => NumCast::from(self.max),
        }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, test_case::test_matrix};

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
}
