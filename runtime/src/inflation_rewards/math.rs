//! Fixed-point helpers for integer inflation.
pub(super) const SCALE: u128 = 1_000_000_000_000_000_000;

// a * b / d, handling u128 overflow via the identity:
//
//   a*b/d = (a/d)*b + (a%d)*b/d
//
// Result must fit within u128, or else this isn't reliable.
pub(super) fn muldiv(a: u128, b: u128, d: u128) -> u128 {
    match a.checked_mul(b) {
        Some(product) => product / d,
        None => {
            let (a, b) = if a >= d { (a, b) } else { (b, a) };
            (a / d) * b + muldiv(a % d, b, d)
        }
    }
}

/// base ** exp via repeated squaring.
pub(super) fn fixed_pow(base_scaled: u128, exp: u128) -> u128 {
    if exp == 0 {
        return SCALE;
    }
    let mut result = SCALE;
    let mut base = base_scaled;
    let mut e = exp;
    while e > 0 {
        if e & 1 == 1 {
            result = result * base / SCALE;
        }
        e >>= 1;
        if e > 0 {
            base = base * base / SCALE;
        }
    }
    result
}

/// ln(x) via the atanh trick.
pub(super) fn fixed_ln(x_scaled: u128) -> i128 {
    debug_assert!((SCALE / 2..=SCALE).contains(&x_scaled));

    let x = x_scaled as i128;
    let s = SCALE as i128;
    let z = (x - s) * s / (x + s);
    let z_sq = z * z / s;

    let mut sum = z;
    let mut power = z;

    for k in 1..=25u64 {
        power = power * z_sq / s;
        let term = power / (2 * k + 1) as i128;
        if term == 0 {
            break;
        }
        sum += term;
    }
    2 * sum
}

/// exp(x) via Taylor series.
pub(super) fn fixed_exp(x_scaled: i128) -> u128 {
    let s = SCALE as i128;
    let mut sum = s;
    let mut term = s;
    for k in 1..=35u64 {
        term = term * x_scaled / (k as i128 * s);
        if term == 0 {
            break;
        }
        sum += term;
    }
    sum.max(0) as u128
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::inflation_rewards::integer_inflation::NANOS_PER_YEAR,
        rand::{Rng, SeedableRng},
        rand_chacha::ChaChaRng,
        solana_clock::{DEFAULT_MS_PER_SLOT, DEFAULT_SLOTS_PER_EPOCH},
    };

    fn to_scaled(v: f64) -> u128 {
        (v * SCALE as f64).round() as u128
    }

    fn abs_diff(a: u128, b: u128) -> u128 {
        (a as i128 - b as i128).unsigned_abs()
    }

    #[test]
    fn test_agave_values() {
        // decay_base = 1.0 - 0.15, where taper = 0.15
        let decay_base = 0.85;
        let base_scaled = to_scaled(decay_base);

        for year in 0..=200u128 {
            let actual = fixed_pow(base_scaled, year);
            let expected = to_scaled(decay_base.powi(year as i32));
            let err = abs_diff(actual, expected);
            assert!(err < 1_000 || err * 1_000_000_000 < expected.max(1));
        }

        let actual = fixed_ln(base_scaled);
        let expected = (decay_base.ln() * SCALE as f64).round() as i128;
        assert!((actual - expected).unsigned_abs() < 100);

        for tenths in 0..=10u64 {
            let arg = (tenths as f64 / 10.0) * decay_base.ln();
            let actual = fixed_exp((arg * SCALE as f64).round() as i128);
            let expected = to_scaled(arg.exp());
            assert!(abs_diff(actual, expected) < 1_000);
        }

        // muldiv with the exact shapes from calculate_validator_rewards_lamports
        let rate = to_scaled(0.076);
        let cap: u128 = 500_000_000_000_000_000;
        let epoch_nanos: u128 = (1_000_000 * DEFAULT_MS_PER_SLOT * DEFAULT_SLOTS_PER_EPOCH).into();

        let rate_cap = muldiv(rate, cap, SCALE);
        assert_eq!(rate_cap, rate * cap / SCALE);

        let actual = muldiv(rate_cap, epoch_nanos, NANOS_PER_YEAR);
        let expected = (0.076 * cap as f64 * epoch_nanos as f64 / NANOS_PER_YEAR as f64) as u128;
        assert!(abs_diff(actual, expected) <= 1);
    }

    #[test]
    fn fuzz_muldiv() {
        let mut rng = ChaChaRng::seed_from_u64(0);
        // a * b fits in u128
        for _ in 0..1_000_000 {
            let a: u128 = rng.random_range(1..=u64::MAX as u128);
            let b: u128 = rng.random_range(1..=u64::MAX as u128);
            let d: u128 = rng.random_range(1..=u64::MAX as u128);
            assert_eq!(muldiv(a, b, d), a * b / d);
        }

        // Force the overflow path. d >= b keeps the quotient in range.
        for _ in 0..1_000_000 {
            let a: u128 = rng.random_range(u64::MAX as u128..=u128::MAX / 2);
            let b: u128 = rng.random_range(2..=1000);
            let d: u128 = rng.random_range(b..=b * 1000);
            let _ = muldiv(a, b, d);
        }
    }

    #[test]
    fn fuzz_fixed_pow() {
        let mut rng = ChaChaRng::seed_from_u64(0);
        for _ in 0..1_000_000 {
            let base_f64: f64 = rng.random_range(0.5..=1.0);
            let base_scaled = (base_f64 * SCALE as f64).round() as u128;
            let exp: u128 = rng.random_range(0..=200);
            let actual = fixed_pow(base_scaled, exp);
            let expected = (base_f64.powf(exp as f64) * SCALE as f64).round() as u128;
            let err = abs_diff(actual, expected);
            assert!(err < 1_000 || err * 1_000_000_000 < expected.max(1));
        }
    }

    #[test]
    fn fuzz_fixed_ln() {
        let mut rng = ChaChaRng::seed_from_u64(0);
        for _ in 0..1_000_000 {
            let x: f64 = rng.random_range(0.5..=1.0);
            let actual = fixed_ln((x * SCALE as f64).round() as u128);
            let expected = (x.ln() * SCALE as f64).round() as i128;
            let err = (actual - expected).unsigned_abs();
            assert!(err * 1_000_000_000 < expected.unsigned_abs().max(1));
        }
    }

    #[test]
    fn fuzz_fixed_exp() {
        let mut rng = ChaChaRng::seed_from_u64(0);
        for _ in 0..1_000_000 {
            let x: f64 = rng.random_range(-2.0..2.0);
            let actual = fixed_exp((x * SCALE as f64).round() as i128);
            let expected = (x.exp() * SCALE as f64).round() as u128;
            let err = abs_diff(actual, expected);
            assert!(err * 1_000_000_000 < expected.max(1));
        }
    }
}
