//! Fixed-point IIR filter for smoothing `alpha` updates.
//!
//! Implements:
//!   alpha_new = K * target + (1 - K) * previous
//!
//! All math is unsigned integer fixed-point with `SCALE = 1000`
//!
//! The filter constant K is derived from:
//!     K = W_C / (1 + W_C), where Wc = 2π * Fc * Fs
//!     Fc = 1 / TC  (cutoff frequency)
//!     Fs = 1 / refresh interval
use crate::cluster_info::REFRESH_PUSH_ACTIVE_SET_INTERVAL_MS;

// Fixed point scale for K and `alpha` calculation
pub const SCALE: u64 = 1_000;
// 30_000ms convergence time
const TC_MS: u64 = 30_000;
// 7500ms refresh interval
const FS_MS: u64 = REFRESH_PUSH_ACTIVE_SET_INTERVAL_MS;
// 2 * pi * SCALE
const TWO_PI_SCALED: u64 = 6_283;
const W_C_SCALED: u64 = (TWO_PI_SCALED * FS_MS) / TC_MS;
const K: u64 = (W_C_SCALED * 1_000 + (SCALE / 2)) / (SCALE + W_C_SCALED);

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
/// Note: Each update is 7500ms apart.
///
/// If future code changes make `alpha_target` jump larger, we must retune
/// `TC`/`K` or use a higher‑order filter to avoid lag/overshoot.
/// Returns `alpha_new = K * target + (1 - K) * prev`, rounded and clamped.
pub fn filter_alpha(prev: u64, target: u64, min: u64, max: u64) -> u64 {
    let updated = (K * target + (SCALE - K) * prev) / SCALE;
    updated.clamp(min, max)
}

/// Approximates `base^alpha` rounded to nearest integer using
/// integer-only linear interpolation between `base^1` and `base^2`.
#[inline]
pub fn interpolate(base: u64, t: u64) -> u64 {
    debug_assert!(t <= SCALE, "interpolation t={} > SCALE={}", t, SCALE);
    let base_squared = base.saturating_mul(base);
    ((base * (SCALE - t) + base_squared * t) + SCALE / 2) / SCALE
}
