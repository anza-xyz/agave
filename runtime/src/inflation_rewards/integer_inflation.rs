//! Fixed-point integer replacement for the f64 inflation reward pipeline.
use {
    super::math::{SCALE, SCALE_SHIFT, ScaledU60, fixed_exp, fixed_ln, fixed_pow, muldiv},
    solana_clock::DEFAULT_MS_PER_SLOT,
    solana_inflation::Inflation,
};

// 365.242199 days in nanoseconds.
pub const NANOS_PER_YEAR: u128 = 31_556_925_993_600_000;
pub const NS_PER_SLOT: u128 = DEFAULT_MS_PER_SLOT as u128 * 1_000_000;

#[derive(Debug, Clone)]
pub(crate) struct IntegerInflation {
    initial_scaled: ScaledU60,
    terminal_scaled: ScaledU60,
    decay_base_scaled: ScaledU60,
    foundation_scaled: ScaledU60,
    foundation_term_nanos: u128,
}

impl From<&Inflation> for IntegerInflation {
    fn from(infl: &Inflation) -> Self {
        Self {
            initial_scaled: f64_to_scaled(infl.initial),
            terminal_scaled: f64_to_scaled(infl.terminal),
            decay_base_scaled: f64_to_scaled(1.0 - infl.taper),
            foundation_scaled: f64_to_scaled(infl.foundation),
            foundation_term_nanos: (infl.foundation_term * NANOS_PER_YEAR as f64).round() as u128,
        }
    }
}

fn f64_to_scaled(v: f64) -> ScaledU60 {
    assert!(
        v >= 0.0 && v.is_finite(),
        "f64_to_scaled: {v} is negative, NaN, or infinite"
    );
    if v == 0.0 {
        return 0;
    }
    let bits = v.to_bits();
    let mantissa = (bits & 0x000F_FFFF_FFFF_FFFF) | 0x0010_0000_0000_0000; // 53-bit with implicit leading 1
    let biased_exp = ((bits >> 52) & 0x7FF) as i32;
    // v = mantissa * 2^(biased_exp - 1023 - 52)
    // v * SCALE = mantissa * 2^(biased_exp - 1023 - 52 + SCALE_SHIFT)
    let shift = biased_exp - 1023 - 52 + SCALE_SHIFT as i32;

    match shift >= 0 {
        true => {
            let shift = shift as u32;
            if shift > 128 - 53 {
                return u128::MAX;
            }
            (mantissa as u128) << shift
        }
        false => {
            let right = (-shift) as u32;
            if right >= 128 {
                return 0;
            }
            // Round to nearest.
            ((mantissa as u128) + (1u128 << (right - 1))) >> right
        }
    }
}

impl IntegerInflation {
    /// max(terminal, initial * (1 - taper) ^ year), scaled.
    pub(crate) fn total_inflation_scaled(&self, num_slots: u64) -> ScaledU60 {
        let year_nanos = num_slots as u128 * NS_PER_SLOT;
        let tapered = self.initial_scaled * self.compute_decay(year_nanos) / SCALE;
        tapered.max(self.terminal_scaled)
    }

    pub(crate) fn validator_inflation_scaled(&self, num_slots: u64) -> ScaledU60 {
        let total = self.total_inflation_scaled(num_slots);
        total - self.foundation_from_total(num_slots, total)
    }

    #[cfg(test)]
    pub(crate) fn foundation_inflation_scaled(&self, num_slots: u64) -> ScaledU60 {
        let total = self.total_inflation_scaled(num_slots);
        self.foundation_from_total(num_slots, total)
    }

    fn foundation_from_total(&self, num_slots: u64, total_scaled: ScaledU60) -> ScaledU60 {
        let year_nanos = num_slots as u128 * NS_PER_SLOT;
        if year_nanos < self.foundation_term_nanos {
            total_scaled * self.foundation_scaled / SCALE
        } else {
            0
        }
    }

    /// Validator rewards in lamports for a single epoch.
    pub(crate) fn calculate_validator_rewards_lamports(
        &self,
        num_slots: u64,
        slots_in_epoch: u64,
        capitalization: u64,
    ) -> u64 {
        let validator_scaled = self.validator_inflation_scaled(num_slots);
        if validator_scaled == 0 || capitalization == 0 || slots_in_epoch == 0 {
            return 0;
        }

        // reward = (validator_scaled / SCALE) * cap * (slots_in_epoch * ns_per_slot / NANOS_PER_YEAR)
        // Split into two muldivs to stay within u128.
        let epoch_nanos = slots_in_epoch as u128 * NS_PER_SLOT;
        let rate_cap = muldiv(validator_scaled, capitalization as u128, SCALE);
        muldiv(rate_cap, epoch_nanos, NANOS_PER_YEAR) as u64
    }

    // (1 - taper)^year as a SCALE-d value, decomposed into integer + fractional year.
    fn compute_decay(&self, year_nanos: u128) -> ScaledU60 {
        if year_nanos == 0 || self.decay_base_scaled == SCALE {
            return SCALE; // no elapsed time, or taper == 0
        }
        if self.decay_base_scaled == 0 {
            return 0; // taper == 1
        }

        let full_years = year_nanos / NANOS_PER_YEAR;
        let remainder = year_nanos % NANOS_PER_YEAR;

        let int_part = fixed_pow(self.decay_base_scaled, full_years);
        if remainder == 0 {
            return int_part;
        }

        // base^frac = exp(frac * ln(base))
        let ln_base = fixed_ln(self.decay_base_scaled);
        let arg = remainder as i128 * ln_base / NANOS_PER_YEAR as i128;
        let frac_part = fixed_exp(arg);

        int_part * frac_part / SCALE
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        rand::{Rng, SeedableRng},
        rand_chacha::ChaChaRng,
        solana_clock::DEFAULT_SLOTS_PER_EPOCH,
        solana_inflation::Inflation,
        test_case::test_matrix,
    };

    const SLOTS_PER_YEAR: f64 = NANOS_PER_YEAR as f64 / NS_PER_SLOT as f64;

    fn to_scaled(v: f64) -> u128 {
        (v * SCALE as f64).round() as u128
    }

    fn abs_diff(a: u128, b: u128) -> u128 {
        a.max(b).saturating_sub(a.min(b))
    }

    #[test]
    fn test_f64_to_scaled() {
        assert_eq!(f64_to_scaled(0.0), 0);
        assert_eq!(f64_to_scaled(1.0), SCALE);

        // powers of two should be exact
        assert_eq!(f64_to_scaled(0.5), SCALE / 2);
        assert_eq!(f64_to_scaled(0.25), SCALE / 4);
        assert_eq!(f64_to_scaled(0.125), SCALE / 8);

        // check against naive f64 multiply (works because 2^60 is exact in f64)
        // and round-trip back to f64
        for v in [0.08, 0.015, 0.85, 0.05, 7.0] {
            assert_eq!(f64_to_scaled(v), (v * SCALE as f64).round() as u128);
            assert_eq!(f64_to_scaled(v) as f64 / SCALE as f64, v);
        }
    }

    #[test]
    fn test_f64_to_scaled_saturates() {
        assert_eq!(f64_to_scaled(4.0e21), u128::MAX);
        assert_eq!(f64_to_scaled(1.0e-30), 0);
    }

    #[test]
    #[should_panic(expected = "f64_to_scaled")]
    fn test_f64_to_scaled_rejects_negative() {
        f64_to_scaled(-1.0);
    }

    #[test]
    fn fuzz_rates_vs_f64() {
        let infl = Inflation::default();
        let ii = IntegerInflation::from(&infl);
        let mut rng = ChaChaRng::seed_from_u64(0);

        for _ in 0..1_000_000 {
            // Compare all three rate functions against the f64 originals at random
            // points in time. All functions must agree to 1 part in 10^15.
            let approx_year: f64 = rng.random_range(0.0..100.0);
            let ns = (approx_year * SLOTS_PER_YEAR).round() as u64;
            let year = ns as f64 / SLOTS_PER_YEAR;

            let expected = to_scaled(infl.total(year));
            let actual = ii.total_inflation_scaled(ns);
            assert!(abs_diff(expected, actual) < 1_000);

            let expected = to_scaled(infl.validator(year));
            let actual = ii.validator_inflation_scaled(ns);
            assert!(abs_diff(expected, actual) < 1_000);

            let expected = to_scaled(infl.foundation(year));
            let actual = ii.foundation_inflation_scaled(ns);
            assert!(abs_diff(expected, actual) < 1_000);
        }
    }

    #[test_matrix(
        [Inflation::default(), Inflation::pico()]
    )]
    fn test_rewards_match(infl: Inflation) {
        let ii = IntegerInflation::from(&infl);
        let cap = 500_000_000_000_000_000u64; // ~500M SOL
        let dur = DEFAULT_SLOTS_PER_EPOCH as f64 / SLOTS_PER_YEAR;

        // 100k epochs is ~210 years. Fingers crossed that this test still matters then.
        for epoch in 0..100_000 {
            let ns = epoch * DEFAULT_SLOTS_PER_EPOCH;
            let actual = ii.calculate_validator_rewards_lamports(ns, DEFAULT_SLOTS_PER_EPOCH, cap);
            let year = ns as f64 / SLOTS_PER_YEAR;
            let expected = (infl.validator(year) * cap as f64 * dur) as u64;
            assert!(abs_diff(expected as u128, actual as u128) <= 1);
        }
    }

    #[test]
    fn test_full_inflation() {
        let ii = IntegerInflation::from(&Inflation::full());
        for epoch in (0..500).step_by(50) {
            let ns = epoch * DEFAULT_SLOTS_PER_EPOCH;
            assert_eq!(
                ii.validator_inflation_scaled(ns),
                ii.total_inflation_scaled(ns)
            );
        }
    }
}
