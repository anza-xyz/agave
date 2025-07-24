#![cfg_attr(not(feature = "agave-unstable-api"), allow(dead_code))]
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
#[cfg(feature = "agave-unstable-api")]
pub mod api {
    use std::num::NonZeroU64;

    // Fixed point scale for K and `alpha` calculation
    pub const SCALE: NonZeroU64 = NonZeroU64::new(1_000_000).unwrap();
    // 2 * pi * SCALE
    const TWO_PI_SCALED: u64 = 6_283_185;

    pub struct FilterConfig {
        pub output_range: std::ops::Range<u64>,
        pub k: u64,
    }

    /// Computes the filter constant `K` for a given sample period and
    /// time‑constant, both in **milliseconds**.
    ///
    /// Returns `K` scaled by `SCALE` (0–1,000,000).
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
    #[inline]
    pub fn interpolate(base: u64, t: u64) -> u64 {
        let scale = SCALE.get();
        debug_assert!(t <= scale, "interpolation t={} > SCALE={}", t, scale);
        let base_squared = base.saturating_mul(base);
        // ((base * (scale - t) + base_squared * t) + scale / 2) / scale
        ((base.saturating_mul(scale.saturating_sub(t)))
            .saturating_add(base_squared.saturating_mul(t)))
        .saturating_add(scale.saturating_div(2))
        .saturating_div(scale)
    }
}

//TODO: greg: add tests for this file
