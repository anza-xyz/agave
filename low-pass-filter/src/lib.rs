#![cfg(feature = "agave-unstable-api")]
//! Fixed-point IIR filter for smoothing `alpha` updates.
//!
//! This is equivalent to a discrete-time Butterworth filter of order 1
//! Implements:
//!   alpha_new = K * target + (1 - K) * previous
//!
//! All math is unsigned integer fixed-point with `SCALE = 1,000,000`
//!
//! The filter constant K is derived from:
//!     K = W_C / (1 + W_C), where Wc = 2π * Fs / Tc
//!     Fc = 1 / TC  (cutoff frequency)
//!     Fs = 1 / refresh interval
pub mod api {
    use std::num::NonZeroU64;

    // Fixed point scale for K and `alpha` calculation
    pub const SCALE: NonZeroU64 = NonZeroU64::new(1_000_000).unwrap();
    // 2 * pi * SCALE
    const TWO_PI_SCALED: u64 = 6_283_185;

    #[derive(Clone)]
    pub struct FilterConfig {
        pub output_range: std::ops::Range<u64>,
        pub k: u64,
    }

    /// Computes the filter constant `K` for a given sample period and
    /// time‑constant, both in **milliseconds**.
    ///
    /// Returns `K` scaled by `SCALE` (0–1,000,000).
    #[allow(clippy::arithmetic_side_effects)]
    pub fn compute_k(fs_ms: u64, tc_ms: u64) -> u64 {
        if tc_ms == 0 {
            return 0;
        }
        let scale = SCALE.get();
        let wc_scaled = (TWO_PI_SCALED.saturating_mul(fs_ms)).saturating_div(tc_ms);
        // ((wc_scaled * scale + scale / 2) / (scale + wc_scaled)).min(scale) rounded to nearest integer
        ((wc_scaled
            .saturating_mul(scale)
            .saturating_add(scale.saturating_div(2)))
        .saturating_div(scale.saturating_add(wc_scaled)))
        .min(scale)
    }

    /// Updates alpha with a first-order low-pass filter.
    /// ### Convergence Characteristics (w/ K = 0.611):
    ///
    /// - From a step change in target, `alpha` reaches:
    ///   - ~61% of the way to target after 1 update
    ///   - ~85% after 2
    ///   - ~94% after 3
    ///   - ~98% after 4
    ///   - ~99% after 5
    ///
    /// Note: Each update is `fs_ms` apart. `fs_ms` is 7500ms for push_active_set.
    ///
    /// If future code changes make `alpha_target` jump larger, we must retune
    /// `TC`/`K` or use a higher‑order filter to avoid lag/overshoot.
    /// Returns `alpha_new = K * target + (1 - K) * prev`, rounded and clamped.
    #[allow(clippy::arithmetic_side_effects)]
    pub fn filter_alpha(prev: u64, target: u64, filter_config: FilterConfig) -> u64 {
        let scale = SCALE.get();
        // (k * target + (scale - k) * prev) / scale
        let next = (filter_config.k.saturating_mul(target))
            .saturating_add((scale.saturating_sub(filter_config.k)).saturating_mul(prev))
            .saturating_div(scale);
        next.clamp(
            filter_config.output_range.start,
            filter_config.output_range.end,
        )
    }

    /// Approximates `base^alpha` rounded to nearest integer using
    /// integer-only linear interpolation between `base^1` and `base^2`.
    ///
    /// Note: This function is most accurate when `base` is small e.g. < ~25.
    #[inline]
    #[allow(clippy::arithmetic_side_effects)]
    pub fn interpolate(base: u64, t: u64) -> u64 {
        let scale = SCALE.get();
        debug_assert!(t <= scale, "interpolation t={} > SCALE={}", t, scale);
        let base_squared = base.saturating_mul(base);
        // ((base * (scale - t) + base_squared * t) + scale / 2) / scale
        ((base.saturating_mul(scale.saturating_sub(t)))
            .saturating_add(base_squared.saturating_mul(t)))
        .saturating_add(scale / 2)
            / scale
    }
}

#[cfg(test)]
mod tests {
    use super::api::*;

    #[test]
    fn test_compute_k_zero_tc() {
        // When time constant is 0, K should be 0
        assert_eq!(compute_k(100, 0), 0);
        assert_eq!(compute_k(1000, 0), 0);
        assert_eq!(compute_k(u64::MAX, 0), 0);
    }

    #[test]
    fn test_compute_k_zero_fs() {
        // When sample frequency is 0, K should be 0
        assert_eq!(compute_k(0, 100), 0);
        assert_eq!(compute_k(0, 1000), 0);
        assert_eq!(compute_k(0, u64::MAX), 0);
    }

    #[test]
    fn test_compute_k_large_values() {
        // K should never exceed SCALE
        let k = compute_k(u64::MAX, 1);
        assert!(k <= SCALE.get());

        let k = compute_k(1000000, 1);
        assert!(k <= SCALE.get());

        let k = compute_k(u64::MAX / 2, u64::MAX / 4);
        assert!(k <= SCALE.get());

        let k = compute_k(500000000, 1000000000);
        assert!(k <= SCALE.get());
    }

    #[test]
    fn test_compute_k_normal_cases() {
        // Test some normal cases
        let k1 = compute_k(100, 1000);
        assert_eq!(k1, 385869);

        let k2 = compute_k(1000, 100);
        assert_eq!(k2, 984333);
        assert!(k2 > k1);

        let k3 = compute_k(1000, 1000);
        assert_eq!(k3, 862697);
    }

    #[test]
    fn test_filter_alpha_k_zero() {
        // When K=0, alpha should not change
        let config = FilterConfig {
            output_range: 0..1000000,
            k: 0,
        };

        assert_eq!(filter_alpha(100, 500, config.clone()), 100);
        assert_eq!(filter_alpha(0, 1000000, config.clone()), 0);
        assert_eq!(filter_alpha(999999, 0, config), 999999);
    }

    #[test]
    fn test_filter_alpha_k_max() {
        // When K=SCALE, alpha should equal target value (clamped to range)
        let config = FilterConfig {
            output_range: 0..1000000,
            k: SCALE.get(),
        };

        assert_eq!(filter_alpha(100, 500, config.clone()), 500);
        assert_eq!(filter_alpha(0, 1000000, config), 1000000);

        // Test clamping - target outside range
        let config = FilterConfig {
            output_range: 100..900,
            k: SCALE.get(),
        };
        assert_eq!(filter_alpha(200, 50, config.clone()), 100);
        assert_eq!(filter_alpha(200, 1000, config), 900);
    }

    #[test]
    fn test_filter_alpha_clamping() {
        // Test output range clamping
        let config = FilterConfig {
            output_range: 100..900,
            k: SCALE.get() / 2,
        };

        // This should be within range
        let result = filter_alpha(950, 50, config);
        assert_eq!(result, 500);

        // Test extreme clamping
        let config_narrow = FilterConfig {
            output_range: 500..501,
            k: SCALE.get() / 4,
        };
        let result = filter_alpha(0, 1000000, config_narrow);
        assert_eq!(result, 501);
    }

    #[test]
    fn test_filter_alpha_overflow_protection() {
        // Test with large values that might cause overflow
        let config = FilterConfig {
            output_range: 0..u64::MAX,
            k: SCALE.get() / 2,
        };

        let result = filter_alpha(u64::MAX / 2, u64::MAX / 2, config.clone());
        assert_eq!(result, 18446744073709);

        let result2 = filter_alpha(u64::MAX - 1000, u64::MAX - 2000, config);
        assert_eq!(result2, 18446744073709);
    }

    #[test]
    fn test_filter_alpha_mathematical_correctness() {
        let config = FilterConfig {
            output_range: 0..u64::MAX,
            k: SCALE.get() / 4, // 25%
        };

        let prev = 800;
        let target = 400;
        let result = filter_alpha(prev, target, config);
        assert_eq!(result, 700);

        let config = FilterConfig {
            output_range: 0..u64::MAX,
            k: SCALE.get() * 60 / 100, // 60%
        };

        let prev = 111111;
        let target = 222222;
        let result = filter_alpha(prev, target, config);
        assert_eq!(result, 177777);
    }

    #[test]
    fn test_interpolate_t_zero() {
        // When t=0, should return base
        assert_eq!(interpolate(100, 0), 100);
        assert_eq!(interpolate(0, 0), 0);
        assert_eq!(interpolate(1000000, 0), 1000000);
    }

    #[test]
    fn test_interpolate_t_max() {
        // When t=SCALE, should return base^2
        let base = 100;
        let result = interpolate(base, SCALE.get());
        assert_eq!(result, base * base);

        let base2 = 1000;
        let result = interpolate(base2, SCALE.get());
        assert_eq!(result, base2 * base2);
    }

    #[test]
    fn test_interpolate_values() {
        let t_10 = SCALE.get() / 10; // 10%
        let t_50 = SCALE.get() / 2; // 50%
        let t_75 = SCALE.get() * 3 / 4; // 75%

        let base = 3;
        let result = interpolate(base, t_10);
        assert_eq!(result, 4);

        let result = interpolate(base, t_50);
        assert_eq!(result, 6);

        let result = interpolate(base, t_75);
        assert_eq!(result, 8);

        let base = 15;
        let result = interpolate(base, t_10);
        assert_eq!(result, 36);

        let result = interpolate(base, t_50);
        assert_eq!(result, 120);

        let result = interpolate(base, t_75);
        assert_eq!(result, 173);

        let base = 24;
        let result = interpolate(base, t_10);
        assert_eq!(result, 79);

        let result = interpolate(base, t_50);
        assert_eq!(result, 300);

        let result = interpolate(base, t_75);
        assert_eq!(result, 438);
    }

    #[test]
    fn test_interpolate_large_base() {
        let base = 1000000000;
        let result = interpolate(base, SCALE.get() / 2);
        assert!(result >= base);
        assert!(result < base * base);
    }

    #[test]
    fn test_interpolate_edge_cases() {
        // Test with base = 1
        assert_eq!(interpolate(1, 0), 1);
        assert_eq!(interpolate(1, SCALE.get()), 1);
        assert_eq!(interpolate(1, SCALE.get() / 2), 1);

        // Test with base = 0
        assert_eq!(interpolate(0, 0), 0);
        assert_eq!(interpolate(0, SCALE.get()), 0);
        assert_eq!(interpolate(0, SCALE.get() / 2), 0);
    }

    #[test]
    fn test_interpolate_rounding() {
        let base = 3;
        let t = SCALE.get() / 3;
        let result = interpolate(base, t);

        assert!(result >= 3);
        assert!(result <= 9);
    }

    #[test]
    fn test_scale_constant() {
        assert_eq!(SCALE.get(), 1_000_000);
        assert!(SCALE.get() > 0);
    }

    #[test]
    fn test_integration_filter_and_interpolate() {
        // Test using filtered alpha with interpolate
        // Alpha range is [SCALE, 2*SCALE] as used in push_active_set
        let alpha_min = SCALE.get();
        let alpha_max = 2 * SCALE.get();

        let config = FilterConfig {
            output_range: alpha_min..alpha_max,
            k: SCALE.get() / 10, // 10%
        };

        let prev_alpha = alpha_min + SCALE.get() / 4; // 1.25 * SCALE
        let target_alpha = alpha_min + SCALE.get() / 2; // 1.5 * SCALE
        let filtered_alpha = filter_alpha(prev_alpha, target_alpha, config);

        let t = filtered_alpha.saturating_sub(alpha_min);
        let base = 2;
        let result = interpolate(base, t);

        assert!(result >= base);
        assert!(result <= base * base);

        assert!(filtered_alpha >= alpha_min);
        assert!(filtered_alpha <= alpha_max);
    }
}
