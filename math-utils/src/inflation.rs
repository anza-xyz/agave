//! Deterministic inflation decay specified by SIMD-0607.
//!
//! Checked failures must be propagated to the validator, which must fail loudly
//! rather than substitute a different inflation schedule.

pub const RATE_SCALE: u64 = 1_000_000_000_000_000;
pub const YEAR_NS: u128 = 31_556_925_993_600_000;
const DECAY_SCALE: u128 = 1 << 64;

/// Applies taper over one slot-time regime.
/// Returns the annual rate without a schedule-specific terminal clamp.
pub fn decayed_rate(
    anchor_rate: u64,
    taper_num: u32,
    taper_den: u32,
    ns_per_slot: u128,
    slots: u64,
) -> Option<u64> {
    let factor = decay_per_slot(ns_per_slot, taper_num, taper_den)?;
    let decay = pow_decay_floor(factor, slots)?;
    apply_decay_floor(anchor_rate, decay)
}

/// Computes rewards with separate annual and epoch flooring operations.
/// The epoch duration is `slots_in_epoch * ns_per_slot / YEAR_NS` years.
pub fn epoch_reward(
    capitalization: u64,
    rate: u64,
    slots_in_epoch: u64,
    ns_per_slot: u128,
) -> Option<u64> {
    if rate > RATE_SCALE || ns_per_slot == 0 {
        return None;
    }
    let annual = u128::from(capitalization)
        .checked_mul(u128::from(rate))?
        .checked_div(u128::from(RATE_SCALE))?;
    let reward = annual
        .checked_mul(u128::from(slots_in_epoch))?
        .checked_mul(ns_per_slot)?
        .checked_div(YEAR_NS)?;
    u64::try_from(reward).ok()
}

/// Approximates DECAY_SCALE * ln(numerator / denominator).
///
/// Narrow inputs are widened before arithmetic. With u32 inputs,
/// `z_num <= u32::MAX` and `z_den <= 2 * u32::MAX`, so their squares fit in
/// u128. Checked operations still make any arithmetic failure explicit.
fn approximate_logarithm(numerator: u32, denominator: u32) -> Option<u128> {
    if denominator == 0 || numerator < denominator {
        return None;
    }
    let numerator = u128::from(numerator);
    let denominator = u128::from(denominator);
    // Represent z = (numerator - denominator) / (numerator + denominator)
    // as a fraction. Then (1 + z) / (1 - z) = numerator / denominator,
    // so ln(numerator / denominator) = 2 * (z + z^3/3 + z^5/5 + ...).
    let z_num = numerator.checked_sub(denominator)?;
    let z_den = numerator.checked_add(denominator)?;
    let z_num_squared = z_num.checked_mul(z_num)?;
    let z_den_squared = z_den.checked_mul(z_den)?;
    // Initially, term is z scaled by DECAY_SCALE.
    let mut term = DECAY_SCALE.checked_mul(z_num)?.checked_div(z_den)?;
    let mut sum = 0u128;
    for i in 0u128..32 {
        let divisor = i.checked_mul(2)?.checked_add(1)?;
        sum = sum.checked_add(term.checked_div(divisor)?)?;
        if i < 31 {
            // Multiply by z^2 to advance to the next scaled odd power.
            term = term
                .checked_mul(z_num_squared)?
                .checked_div(z_den_squared)?;
        }
    }
    sum.checked_mul(2)
}

/// Approximates DECAY_SCALE * exp(-x / DECAY_SCALE), including the leading 1.
fn approximate_exponential(x: u128) -> Option<u128> {
    let second = x.checked_mul(x)?.checked_div(DECAY_SCALE.checked_mul(2)?)?;
    let third = second
        .checked_mul(x)?
        .checked_div(DECAY_SCALE.checked_mul(3)?)?;
    let decrement = x.checked_sub(second)?.checked_add(third)?;
    DECAY_SCALE.checked_sub(decrement)
}

/// Derives a Q64 per-slot decay factor using the SIMD-0607 integer algorithm.
///
/// Returns None for zero slot duration, invalid taper, or arithmetic overflow.
/// The term counts and division positions are part of the consensus algorithm.
fn decay_per_slot(ns: u128, taper_num: u32, taper_den: u32) -> Option<u128> {
    if ns == 0 || taper_den == 0 || taper_num >= taper_den {
        return None;
    }
    // -ln(1 - taper) = ln(taper_den / (taper_den - taper_num)).
    let annual_exponent = approximate_logarithm(taper_den, taper_den.checked_sub(taper_num)?)?;
    let slot_exponent = annual_exponent.checked_mul(ns)?.checked_div(YEAR_NS)?;
    approximate_exponential(slot_exponent)
}

/// Multiplies Q64 factors, flooring the product after division by DECAY_SCALE.
/// The identity is handled separately because DECAY_SCALE squared exceeds u128.
fn mul_decay_floor(a: u128, b: u128) -> Option<u128> {
    if a > DECAY_SCALE || b > DECAY_SCALE {
        return None;
    }
    if a == DECAY_SCALE {
        return Some(b);
    }
    if b == DECAY_SCALE {
        return Some(a);
    }
    a.checked_mul(b)?.checked_shr(64)
}

/// Uses the specified right-to-left binary exponentiation association order and
/// floors after each multiplication.
fn pow_decay_floor(mut base: u128, mut slots: u64) -> Option<u128> {
    if base > DECAY_SCALE {
        return None;
    }
    let mut result = DECAY_SCALE;
    while slots > 0 {
        if slots & 1 != 0 {
            result = mul_decay_floor(result, base)?;
        }
        slots = slots.checked_shr(1)?;
        if slots > 0 {
            base = mul_decay_floor(base, base)?;
        }
    }
    Some(result)
}

/// Applies accumulated Q64 decay without first rounding it to RATE_SCALE.
fn apply_decay_floor(anchor_rate: u64, decay: u128) -> Option<u64> {
    if anchor_rate > RATE_SCALE || decay > DECAY_SCALE {
        return None;
    }
    let rate = u128::from(anchor_rate)
        .checked_mul(decay)?
        .checked_shr(64)?;
    u64::try_from(rate).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_RATE: u64 = 80_000_000_000_000;

    fn assert_decay_matches_float(ns: u128, taper_num: u32) {
        let actual = decay_per_slot(ns, taper_num, 100).unwrap() as f64 / DECAY_SCALE as f64;
        let taper = f64::from(taper_num) / 100.0;
        let expected = (1.0 - taper).powf(ns as f64 / YEAR_NS as f64);
        // f64 resolves only about 53 of the factor's 64 fractional bits.
        assert!(
            (actual - expected).abs() <= 2.0 * f64::EPSILON,
            "ns={ns}, taper={taper_num}: actual={actual}, expected={expected}"
        );
    }

    #[test]
    fn factor_approximates_float_reference() {
        assert_decay_matches_float(400_000_000, 15);
        assert_decay_matches_float(400_000_000, 30);
        assert_decay_matches_float(350_000_000, 15);
        assert_decay_matches_float(350_000_000, 30);
        assert_decay_matches_float(300_000_000, 15);
        assert_decay_matches_float(300_000_000, 30);
        assert_decay_matches_float(250_000_000, 15);
        assert_decay_matches_float(250_000_000, 30);
        assert_decay_matches_float(200_000_000, 15);
        assert_decay_matches_float(200_000_000, 30);
        assert_decay_matches_float(1_000_000, 15);
        assert_decay_matches_float(1_000_000, 30);
        assert_decay_matches_float(6_400_001, 15);
        assert_decay_matches_float(6_400_001, 30);
        assert_decay_matches_float(2_000_000_000, 15);
        assert_decay_matches_float(2_000_000_000, 30);
    }

    #[test]
    fn invalid_inputs_and_overflow() {
        assert_eq!(decay_per_slot(0, 15, 100), None);
        assert_eq!(decay_per_slot(400_000_000, 0, 0), None);
        assert_eq!(decay_per_slot(400_000_000, 100, 100), None);
        assert_eq!(decay_per_slot(400_000_000, 101, 100), None);
        assert_eq!(decay_per_slot(u128::MAX, 15, 100), None);
        assert_eq!(approximate_logarithm(1, 0), None);
        assert_eq!(approximate_logarithm(1, 2), None);
        assert_eq!(
            approximate_logarithm(u32::MAX, 1),
            Some(100_152_660_084_164_962_286)
        );
        assert_eq!(approximate_logarithm(u32::MAX, u32::MAX), Some(0));
        assert_eq!(decay_per_slot(400_000_000, 0, 100), Some(DECAY_SCALE));
    }

    #[test]
    fn accumulated_decay_vectors() {
        assert_eq!(mul_decay_floor(DECAY_SCALE, DECAY_SCALE), Some(DECAY_SCALE));
        assert_eq!(mul_decay_floor(DECAY_SCALE, 0), Some(0));
        assert_eq!(mul_decay_floor(0, DECAY_SCALE), Some(0));
        assert_eq!(mul_decay_floor(u128::MAX, 0), None);
        assert_eq!(
            mul_decay_floor(18_446_744_073_709_551_615, 18_446_744_073_709_551_615),
            Some(18_446_744_073_709_551_614)
        );
        assert_eq!(pow_decay_floor(0, 0), Some(DECAY_SCALE));
        assert_eq!(pow_decay_floor(DECAY_SCALE, u64::MAX), Some(DECAY_SCALE));
        assert_eq!(pow_decay_floor(u128::MAX, 0), None);
        let factor = decay_per_slot(400_000_000, 15, 100).unwrap();
        assert_eq!(
            pow_decay_floor(factor, 432_000),
            Some(18_430_335_172_813_264_384)
        );
        assert_eq!(pow_decay_floor(factor, u64::MAX), Some(0));
    }

    #[test]
    fn rates_and_rewards() {
        assert_eq!(
            decayed_rate(TEST_RATE, 15, 100, 400_000_000, 0),
            Some(TEST_RATE)
        );
        assert_eq!(
            decayed_rate(TEST_RATE, 15, 100, 400_000_000, 432_000),
            Some(79_928_837_736_000)
        );
        assert_eq!(decayed_rate(TEST_RATE, 15, 0, 400_000_000, 0), None);
        assert_eq!(
            epoch_reward(100, RATE_SCALE.checked_div(10).unwrap(), 11, YEAR_NS / 2),
            Some(55)
        );
        assert_eq!(apply_decay_floor(TEST_RATE, DECAY_SCALE), Some(TEST_RATE));
        assert_eq!(apply_decay_floor(TEST_RATE, 0), Some(0));
        // Preserve the anchor at zero elapsed slots.
        let anchor = apply_decay_floor(TEST_RATE, 18_430_335_172_812_832_776).unwrap();
        assert_eq!(anchor, 79_928_837_735_998);
        assert_eq!(
            apply_decay_floor(
                anchor,
                pow_decay_floor(decay_per_slot(400_000_000, 30, 100).unwrap(), 0).unwrap()
            ),
            Some(anchor)
        );
        // A single combined floor would return 2 instead of 1 here.
        assert_eq!(
            epoch_reward(3, RATE_SCALE.checked_div(2).unwrap(), 3, YEAR_NS / 2),
            Some(1)
        );
        assert_eq!(
            epoch_reward(u64::MAX, RATE_SCALE, u64::MAX, u128::MAX),
            None
        );
        assert_eq!(epoch_reward(u64::MAX, RATE_SCALE, 2, u128::MAX), None);
        assert_eq!(epoch_reward(100, TEST_RATE, 432_000, 0), None);
    }
}
