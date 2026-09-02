//! Fixed-point helpers for the SIMD-0607 epoch-inflation reward path.
//!
//! The legacy inflation governor stores rates as `f64` values and evaluates
//! the curve with floating-point operations.  SIMD-0607 moves the
//! consensus-critical reward path to fixed-point integer arithmetic, so this
//! module deliberately recognizes only protocol-defined schedules and
//! slot-time regimes.  Returning `None` is not a request to approximate or
//! derive a value another way; it means the caller has supplied an inflation
//! shape or timing regime that is not encoded in the fixed-point protocol
//! tables.
//!
//! SIMD-0607's arithmetic model is deliberately small: percentages and ratios
//! are protocol fractions, multiplication uses a widened intermediate at least
//! 128 bits wide, and integer division rounds toward zero.  Differences from
//! the legacy floating-point calculation are expected at rounding boundaries;
//! the goal is deterministic cross-client agreement, not bit-for-bit emulation
//! of the previous `f64` path.
//!
//! Activation is also part of the consensus rule.  If SIMD-0607 activates on
//! the epoch E -> E+1 boundary, rewards paid for epoch E during E+1 use this
//! fixed-point path.  Banks before activation continue to use the legacy path.
//!
//! Keep this module boring and literal.  The integer constants below are
//! consensus inputs, not optimization opportunities.  In particular, do not
//! recompute the decay constants from `f64`, do not replace the exponentiation
//! order with a mathematically equivalent-looking form, and do not change
//! floor placement without changing the SIMD/test vectors at the same time.

use {
    crate::slot_params::{
        LEGACY_SLOT_PARAMS, SLOT_PARAMS_200MS, SLOT_PARAMS_250MS, SLOT_PARAMS_300MS,
        SLOT_PARAMS_350MS, SlotParams,
    },
    agave_feature_set as feature_set,
    solana_clock::Slot,
    solana_inflation::Inflation,
};

/// Decimal fixed-point scale for inflation rates and decay multipliers.
///
/// `RATE_SCALE` represents `1.0`.  For example, `INITIAL_RATE` represents
/// `0.08`, `TERMINAL_RATE` represents `0.015`, and `PICO_RATE` represents
/// `0.0001`.  The scale is large enough to carry the SIMD-0607 decay table
/// without using floating-point arithmetic in the reward calculation.  These
/// are the integer forms of the existing schedule constants named by the SIMD,
/// not values converted from `Inflation` dynamically.
pub(crate) const RATE_SCALE: u64 = 1_000_000_000_000_000;
pub(crate) const INITIAL_RATE: u64 = 80_000_000_000_000;
pub(crate) const TERMINAL_RATE: u64 = 15_000_000_000_000;
pub(crate) const PICO_RATE: u64 = 100_000_000_000;

const ZERO_RATE: u64 = 0;
const DEFAULT_TAPER_PERCENT: u64 = 15;
const DOUBLE_TAPER_PERCENT: u64 = 30;

/// Rational slots-per-year used for prorating annual rewards.
///
/// The legacy path stores slots/year as `f64`.  The fixed-point path uses a
/// numerator and denominator so the prorating step is integer-only and has one
/// explicit floor point.  These values are table entries paired with
/// `SlotTimeRegime`, not values to derive from nanoseconds per slot at runtime.
/// This mirrors SIMD-0607's `Fraction { numerator, denominator }` model for
/// protocol percentages and ratios.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SlotsPerYear {
    numerator: u64,
    denominator: u64,
}

impl SlotsPerYear {
    const fn new(numerator: u64, denominator: u64) -> Self {
        Self {
            numerator,
            denominator,
        }
    }

    pub(crate) const fn numerator(self) -> u64 {
        self.numerator
    }

    pub(crate) const fn denominator(self) -> u64 {
        self.denominator
    }
}

/// Disinflation schedules understood by the fixed-point reward path.
///
/// `FifteenPercent` is the original Solana taper.  `ThirtyPercent` is the
/// doubled taper used after the double-disinflation feature becomes effective.
/// Each taper has separate per-slot decay constants for each supported
/// slot-time regime.  In SIMD terms, these correspond to `TAPER_15 =
/// 15 / 100` and `TAPER_30 = 30 / 100`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Taper {
    FifteenPercent,
    ThirtyPercent,
}

/// Compact classification of the legacy `Inflation` governor.
///
/// The bank still carries an `Inflation` value for serialization, RPC display,
/// and pre-SIMD behavior.  After SIMD-0607, reward calculation first classifies
/// that value into one of the fixed-point protocol shapes below.  Unknown
/// shapes stay unknown; they must not be rounded into the nearest known table
/// value.  Once classified, callers use these integer schedule kinds rather
/// than calling `Inflation::validator()` in the reward path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InflationKind {
    Disabled,
    Fixed { validator_rate: u64 },
    Tapered,
}

/// Slot-time regimes with SIMD-0607 reward constants.
///
/// Slot-time reductions change both epoch wall-clock duration and the
/// per-slot decay needed to produce the intended annual taper.  This enum
/// intentionally covers only the five regimes for which the protocol has
/// explicit fixed-point table values.  Adding a new slot-time regime requires
/// extending both the slots/year fraction table and the per-slot decay table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SlotTimeRegime {
    Ms400,
    Ms350,
    Ms300,
    Ms250,
    Ms200,
}

impl SlotTimeRegime {
    /// Returns the protocol slots/year ratio for this regime.
    ///
    /// These are the decimal `slots_per_year` values from `SlotParams`
    /// represented as exact rational numbers, preserving the existing protocol
    /// values named by SIMD-0607.  Keeping the denominator explicit avoids
    /// depending on how a platform parses or rounds the corresponding decimal
    /// into binary64.
    const fn slots_per_year(self) -> SlotsPerYear {
        match self {
            Self::Ms400 => SlotsPerYear::new(78_892_314_984, 1_000),
            Self::Ms350 => SlotsPerYear::new(90_162_645_696, 1_000),
            Self::Ms300 => SlotsPerYear::new(105_189_753_312, 1_000),
            Self::Ms250 => SlotsPerYear::new(126_227_703_974, 1_000),
            Self::Ms200 => SlotsPerYear::new(157_784_629_968, 1_000),
        }
    }

    /// Returns the per-slot decay multiplier for this slot-time/taper pair.
    ///
    /// SIMD-0607 makes these rounded values part of the protocol.  They encode
    /// `round_to_nearest_even((1 - taper)^(1 / slots_per_year) * RATE_SCALE)`
    /// for each supported slot duration and taper.  Do not recompute them with
    /// floating-point arithmetic in the reward path; different libm behavior or
    /// rounding would be consensus-visible.
    const fn decay_per_slot(self, taper: Taper) -> u64 {
        match (self, taper) {
            (Self::Ms400, Taper::FifteenPercent) => 999_999_997_939_990,
            (Self::Ms350, Taper::FifteenPercent) => 999_999_998_197_492,
            (Self::Ms300, Taper::FifteenPercent) => 999_999_998_454_993,
            (Self::Ms250, Taper::FifteenPercent) => 999_999_998_712_494,
            (Self::Ms200, Taper::FifteenPercent) => 999_999_998_969_995,
            (Self::Ms400, Taper::ThirtyPercent) => 999_999_995_478_965,
            (Self::Ms350, Taper::ThirtyPercent) => 999_999_996_044_094,
            (Self::Ms300, Taper::ThirtyPercent) => 999_999_996_609_224,
            (Self::Ms250, Taper::ThirtyPercent) => 999_999_997_174_353,
            (Self::Ms200, Taper::ThirtyPercent) => 999_999_997_739_482,
        }
    }
}

/// Returns the tabled slots/year ratio for a `SlotParams` value.
///
/// `None` means the params do not exactly match a SIMD-0607 slot-time regime.
/// The caller should treat that as an unsupported consensus configuration once
/// fixed-point rewards are mandatory.
pub(crate) fn slots_per_year(params: SlotParams) -> Option<SlotsPerYear> {
    slot_time_regime(params).map(SlotTimeRegime::slots_per_year)
}

/// Returns the tabled per-slot decay for a `SlotParams` and taper.
///
/// This is intentionally a lookup through `slot_time_regime`; deriving a decay
/// from `params.slots_per_year()` would reintroduce runtime floating-point
/// behavior that SIMD-0607 is removing from reward calculation.
pub(crate) fn decay_per_slot(params: SlotParams, taper: Taper) -> Option<u64> {
    slot_time_regime(params).map(|regime| regime.decay_per_slot(taper))
}

/// Classifies a legacy `Inflation` value into a fixed-point protocol schedule.
///
/// The accepted shapes mirror the schedules that can appear on live clusters:
/// disabled inflation, pico/fixed inflation, and the full tapered schedule
/// using either the original 15% taper or the double-disinflation 30% taper.
/// Custom foundation allocations or arbitrary rate/taper values are rejected
/// because this module has no SIMD-0607 integer recipe for them.
pub(crate) fn inflation_kind(
    inflation: &Inflation,
    full_inflation_active: bool,
) -> Option<InflationKind> {
    // Full-inflation feature activation is bank/feature-set state, not just a
    // property of the serialized `Inflation` fields.  Once active, the reward
    // path uses the canonical tapered schedule.
    if full_inflation_active {
        return Some(InflationKind::Tapered);
    }

    // Disabled inflation pays no validator rewards.  The taper field is
    // irrelevant in this case because both endpoints and foundation allocation
    // are zero.
    if matches_rate(inflation.initial, ZERO_RATE)
        && matches_rate(inflation.terminal, ZERO_RATE)
        && matches_rate(inflation.foundation, ZERO_RATE)
    {
        return Some(InflationKind::Disabled);
    }

    // Fixed schedules use the same validator rate forever.  Only known legacy
    // rate literals are accepted so their fixed-point representation is
    // explicit and stable.
    if inflation.initial.to_bits() == inflation.terminal.to_bits()
        && matches_rate(inflation.foundation, ZERO_RATE)
    {
        return rate_to_scaled(inflation.initial)
            .map(|validator_rate| InflationKind::Fixed { validator_rate });
    }

    // The tapered schedule is represented by the canonical initial and
    // terminal rates plus one of the protocol tapers.  Foundation inflation
    // changes `validator(year)` to `total(year) - foundation(year)`, and there
    // is currently no SIMD-0607 fixed-point foundation calculation here.
    let uses_default_taper = matches_taper(inflation.taper, DEFAULT_TAPER_PERCENT);
    let uses_double_taper = matches_taper(inflation.taper, DOUBLE_TAPER_PERCENT);
    if matches_rate(inflation.initial, INITIAL_RATE)
        && matches_rate(inflation.terminal, TERMINAL_RATE)
        && (uses_default_taper || uses_double_taper)
    {
        return if matches_rate(inflation.foundation, ZERO_RATE) {
            Some(InflationKind::Tapered)
        } else {
            None
        };
    }

    None
}

/// Computes the epoch's validator inflation budget.
///
/// The order of operations is part of the fixed-point definition:
///
/// 1. floor `capitalization * validator_rate / RATE_SCALE` to get the annual
///    validator reward;
/// 2. prorate that annual amount by the tabled `slots_in_epoch / slots_per_year`
///    ratio;
/// 3. floor once more through integer division.
///
/// SIMD-0607 calls this two-step flooring normative.  Moving the first floor
/// after proration would usually differ by lamports, so keep the structure
/// intact.
pub(crate) fn epoch_reward(
    capitalization: u64,
    validator_rate: u64,
    slots_in_epoch: u64,
    slots_per_year: SlotsPerYear,
) -> u64 {
    // SIMD-0607 specifies two floors: first annualize by the scaled validator
    // rate, then prorate the annual reward by the exact slots/year fraction.
    let annual_reward = u128::from(mul_scaled_floor(capitalization, validator_rate));
    annual_reward
        .checked_mul(u128::from(slots_in_epoch))
        .and_then(|reward| reward.checked_mul(u128::from(slots_per_year.denominator())))
        .map(|reward| reward / u128::from(slots_per_year.numerator()))
        .and_then(|reward| reward.try_into().ok())
        .expect("protocol inflation reward calculation must fit in u64")
}

/// Applies elapsed taper decay to an anchor rate and enforces the terminal rate.
///
/// This corresponds to the legacy `max(terminal, initial * decay)` shape, but
/// with `initial * decay` evaluated as a scaled integer multiply with an
/// immediate floor.
pub(crate) fn tapered_validator_rate(anchor_rate: u64, decay_since_anchor: u64) -> u64 {
    mul_scaled_floor(anchor_rate, decay_since_anchor).max(TERMINAL_RATE)
}

/// Raises a scaled per-slot decay multiplier to a slot count.
///
/// SIMD-0607 specifies right-to-left binary exponentiation because every
/// multiply floors back to `RATE_SCALE`.  With flooring, multiplication is not
/// associative in the way real-number arithmetic is: changing the exponentiation
/// order can change the final integer result.  Do not replace this with `pow`,
/// repeated multiplication, or a left-to-right algorithm unless the protocol
/// definition and vectors change with it.
pub(crate) fn pow_scaled_floor(mut base: u64, mut slots: Slot) -> u64 {
    // Right-to-left binary exponentiation is part of the SIMD-0607 consensus
    // definition because `mul_scaled_floor` truncates after each multiply.
    let mut result = RATE_SCALE;
    while slots > 0 {
        if slots & 1 == 1 {
            result = mul_scaled_floor(result, base);
        }
        slots /= 2;
        if slots > 0 {
            base = mul_scaled_floor(base, base);
        }
    }
    result
}

/// Multiplies two scaled fixed-point values and floors the result.
///
/// The `u128` intermediate prevents overflow for protocol-sized inputs, and
/// the final division by `RATE_SCALE` is the single floor point for one scaled
/// multiplication.  That floor is observable in reward outputs, so callers must
/// not combine multiple multiplications before dividing.  This is the scaled
/// version of SIMD-0607's `mul_fraction_floor`, with the same widened
/// intermediate and toward-zero rounding requirement.
pub(crate) fn mul_scaled_floor(a: u64, b: u64) -> u64 {
    ((u128::from(a) * u128::from(b)) / u128::from(RATE_SCALE))
        .try_into()
        .expect("scaled multiplication must fit in u64")
}

/// Maps runtime slot params to a protocol slot-time regime.
///
/// Matching is exact: both nanoseconds per slot and the binary64
/// `slots_per_year` bits must match the table.  This intentionally rejects
/// custom genesis timing and partially-matching values, because no fixed-point
/// slots/year ratio or decay constant is defined for them.
fn slot_time_regime(params: SlotParams) -> Option<SlotTimeRegime> {
    [
        (LEGACY_SLOT_PARAMS, SlotTimeRegime::Ms400),
        (SLOT_PARAMS_350MS, SlotTimeRegime::Ms350),
        (SLOT_PARAMS_300MS, SlotTimeRegime::Ms300),
        (SLOT_PARAMS_250MS, SlotTimeRegime::Ms250),
        (SLOT_PARAMS_200MS, SlotTimeRegime::Ms200),
    ]
    .into_iter()
    .find_map(|(known_params, regime)| {
        (params.ns_per_slot == known_params.ns_per_slot
            && params.slots_per_year.to_bits() == known_params.slots_per_year.to_bits())
        .then_some(regime)
    })
}

/// Checks that a legacy f64 rate is exactly one of the protocol literals.
fn matches_rate(rate: f64, scaled_rate: u64) -> bool {
    rate_to_scaled(rate) == Some(scaled_rate)
}

/// Checks that a legacy f64 taper is exactly one of the protocol tapers.
///
/// `to_bits` comparisons are intentional.  The legacy governor stores binary64
/// values, and accepting approximate equality here would silently admit custom
/// schedules that do not have fixed-point table entries.
fn matches_taper(taper: f64, taper_percent: u64) -> bool {
    match taper_percent {
        DEFAULT_TAPER_PERCENT => taper.to_bits() == 0.15f64.to_bits(),
        DOUBLE_TAPER_PERCENT => {
            taper.to_bits() == feature_set::double_disinflation_rate::TAPER.to_bits()
        }
        _ => false,
    }
}

/// Converts known legacy f64 rate literals into SIMD-0607 scaled integers.
///
/// This is a whitelist, not a general converter.  A general `rate * RATE_SCALE`
/// conversion would put floating-point arithmetic back into schedule
/// classification and would make arbitrary custom rates appear supported.
fn rate_to_scaled(rate: f64) -> Option<u64> {
    [
        (0.0f64, ZERO_RATE),
        (0.0001f64, PICO_RATE),
        (0.015f64, TERMINAL_RATE),
        (0.08f64, INITIAL_RATE),
    ]
    .into_iter()
    .find_map(|(known_rate, scaled_rate)| {
        (rate.to_bits() == known_rate.to_bits()).then_some(scaled_rate)
    })
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::slot_params::{
            LEGACY_SLOT_PARAMS, SLOT_PARAMS_200MS, SLOT_PARAMS_250MS, SLOT_PARAMS_300MS,
            SLOT_PARAMS_350MS,
        },
    };

    #[test]
    fn test_decay_per_slot_vectors() {
        let cases = [
            (LEGACY_SLOT_PARAMS, 999_999_997_939_990, 999_999_995_478_965),
            (SLOT_PARAMS_350MS, 999_999_998_197_492, 999_999_996_044_094),
            (SLOT_PARAMS_300MS, 999_999_998_454_993, 999_999_996_609_224),
            (SLOT_PARAMS_250MS, 999_999_998_712_494, 999_999_997_174_353),
            (SLOT_PARAMS_200MS, 999_999_998_969_995, 999_999_997_739_482),
        ];
        for (params, expected_15, expected_30) in cases {
            assert_eq!(
                decay_per_slot(params, Taper::FifteenPercent),
                Some(expected_15)
            );
            assert_eq!(
                decay_per_slot(params, Taper::ThirtyPercent),
                Some(expected_30)
            );
        }
    }

    #[test]
    fn test_pow_scaled_floor_vectors() {
        let base = 999_999_997_939_990;
        assert_eq!(pow_scaled_floor(base, 0), RATE_SCALE);
        assert_eq!(pow_scaled_floor(base, 1), base);
        assert_eq!(pow_scaled_floor(base, 2), 999_999_995_879_980);
        assert_eq!(pow_scaled_floor(base, 432_000), 999_110_471_524_374);
    }

    #[test]
    fn test_epoch_reward_two_step_floor_vectors() {
        let capitalization = 1_000_000_000_000_000_000;
        let slots_per_epoch = 432_000;
        assert_eq!(
            epoch_reward(
                capitalization,
                INITIAL_RATE,
                slots_per_epoch,
                slots_per_year(LEGACY_SLOT_PARAMS).unwrap(),
            ),
            438_065_482_132_309
        );

        let almost_one_year_decay = pow_scaled_floor(
            decay_per_slot(LEGACY_SLOT_PARAMS, Taper::FifteenPercent).unwrap(),
            78_892_314,
        );
        assert_eq!(
            tapered_validator_rate(INITIAL_RATE, almost_one_year_decay),
            67_999_997_954_973
        );
    }

    #[test]
    fn test_terminal_rate_clamp() {
        let decay = pow_scaled_floor(
            decay_per_slot(LEGACY_SLOT_PARAMS, Taper::ThirtyPercent).unwrap(),
            78_892_314 * 20,
        );
        assert_eq!(tapered_validator_rate(INITIAL_RATE, decay), TERMINAL_RATE);
    }
}
