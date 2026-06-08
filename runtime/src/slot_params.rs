use {
    agave_feature_set::{self as feature_set, FeatureSet},
    solana_clock::Slot,
    solana_cost_model::cost_tracker::CostTrackerLimits,
    solana_epoch_schedule::EpochSchedule,
    solana_pubkey::Pubkey,
    std::{
        collections::BTreeMap,
        ops::Bound::{Excluded, Included},
    },
};

pub const DEFAULT_MAX_ENTRY_BYTES_PER_SLOT: u64 = 20 * 1024 * 1024; // 20 MiB

/// Runtime parameters tied to a slot-time regime.
///
/// Slot-time reduction features select explicit table values instead of
/// deriving ratios at runtime, which avoids rounding drift in consensus paths.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SlotParams {
    pub(crate) ns_per_slot: u128,
    pub(crate) slots_per_year: f64,
    pub(crate) hashes_per_tick: Option<u64>,
    pub(crate) cost_tracker_limits: CostTrackerLimits,
    pub(crate) max_data_shreds_per_slot: u32,
    pub(crate) max_code_shreds_per_slot: u32,
    pub(crate) max_entry_bytes_per_slot: u64,
    pub(crate) partitioned_epoch_rewards_stake_account_stores_per_block: u64,
}

impl SlotParams {
    /// Builds the slot-0 params baseline from genesis-derived Bank fields.
    ///
    /// Genesis can customize timing values. Bank construction can also
    /// customize the partitioned-rewards write budget. Values without an
    /// explicit construction-time knob retain the legacy baseline.
    pub(crate) fn genesis_baseline(
        ns_per_slot: u128,
        slots_per_year: f64,
        hashes_per_tick: Option<u64>,
        partitioned_epoch_rewards_stake_account_stores_per_block: u64,
    ) -> Self {
        Self {
            ns_per_slot,
            slots_per_year,
            hashes_per_tick,
            partitioned_epoch_rewards_stake_account_stores_per_block,
            ..LEGACY_SLOT_PARAMS
        }
    }

    /// Nanoseconds per slot for these params.
    pub const fn ns_per_slot(&self) -> u128 {
        self.ns_per_slot
    }

    /// Slots per year for these params.
    pub const fn slots_per_year(&self) -> f64 {
        self.slots_per_year
    }

    /// PoH hashes per tick for these params.
    pub const fn hashes_per_tick(&self) -> Option<u64> {
        self.hashes_per_tick
    }

    /// Maximum data shred index capacity for these params.
    pub const fn max_data_shreds_per_slot(&self) -> u32 {
        self.max_data_shreds_per_slot
    }

    /// Maximum coding shred index capacity for these params.
    pub const fn max_code_shreds_per_slot(&self) -> u32 {
        self.max_code_shreds_per_slot
    }

    /// Maximum bytes that can be reserved for entries in one slot.
    pub const fn max_entry_bytes_per_slot(&self) -> u64 {
        self.max_entry_bytes_per_slot
    }

    /// Number of stake account reward stores allowed in one block.
    pub const fn partitioned_epoch_rewards_stake_account_stores_per_block(&self) -> u64 {
        self.partitioned_epoch_rewards_stake_account_stores_per_block
    }

    /// Returns the per-bank cost limits for these params.
    pub(crate) const fn cost_limits(self, raise_block_limits_to_100m: bool) -> CostTrackerLimits {
        let cost_tracker_limits = self.cost_tracker_limits;
        let (account_cost, block_cost) = if raise_block_limits_to_100m {
            (
                cost_tracker_limits
                    .account_cost
                    .saturating_mul(100)
                    .saturating_div(60),
                cost_tracker_limits
                    .block_cost
                    .saturating_mul(100)
                    .saturating_div(60),
            )
        } else {
            (
                cost_tracker_limits.account_cost,
                cost_tracker_limits.block_cost,
            )
        };

        CostTrackerLimits::new(
            account_cost,
            block_cost,
            cost_tracker_limits.allocated_data_size,
        )
    }
}

pub const LEGACY_HASHES_PER_TICK: u64 = 62_500;
pub(crate) const LEGACY_SLOT_PARAMS: SlotParams = SlotParams {
    ns_per_slot: 400_000_000,
    slots_per_year: 78_892_314.984,
    hashes_per_tick: Some(LEGACY_HASHES_PER_TICK),
    cost_tracker_limits: CostTrackerLimits::new(24_000_000, 60_000_000, 100_000_000),
    max_data_shreds_per_slot: 32_768,
    max_code_shreds_per_slot: 32_768,
    max_entry_bytes_per_slot: 20 * 1024 * 1024,
    partitioned_epoch_rewards_stake_account_stores_per_block: 4096,
};

pub(crate) const SLOT_PARAMS_350MS: SlotParams = SlotParams {
    ns_per_slot: 350_000_000,
    slots_per_year: 90_162_645.696,
    hashes_per_tick: Some(54_687),
    cost_tracker_limits: CostTrackerLimits::new(21_000_000, 52_500_000, 87_500_000),
    max_data_shreds_per_slot: 28_672,
    max_code_shreds_per_slot: 28_672,
    max_entry_bytes_per_slot: 18_350_080,
    partitioned_epoch_rewards_stake_account_stores_per_block: 3_584,
};

pub(crate) const SLOT_PARAMS_300MS: SlotParams = SlotParams {
    ns_per_slot: 300_000_000,
    slots_per_year: 105_189_753.312,
    hashes_per_tick: Some(46_875),
    cost_tracker_limits: CostTrackerLimits::new(18_000_000, 45_000_000, 75_000_000),
    max_data_shreds_per_slot: 24_576,
    max_code_shreds_per_slot: 24_576,
    max_entry_bytes_per_slot: 15_728_640,
    partitioned_epoch_rewards_stake_account_stores_per_block: 3_072,
};

pub(crate) const SLOT_PARAMS_250MS: SlotParams = SlotParams {
    ns_per_slot: 250_000_000,
    slots_per_year: 126_227_703.974,
    hashes_per_tick: Some(39_062),
    cost_tracker_limits: CostTrackerLimits::new(15_000_000, 37_500_000, 62_500_000),
    max_data_shreds_per_slot: 20_480,
    max_code_shreds_per_slot: 20_480,
    max_entry_bytes_per_slot: 13_107_200,
    partitioned_epoch_rewards_stake_account_stores_per_block: 2_560,
};

pub(crate) const SLOT_PARAMS_200MS: SlotParams = SlotParams {
    ns_per_slot: 200_000_000,
    slots_per_year: 157_784_629.968,
    hashes_per_tick: Some(31_250),
    cost_tracker_limits: CostTrackerLimits::new(12_000_000, 30_000_000, 50_000_000),
    max_data_shreds_per_slot: 16_384,
    max_code_shreds_per_slot: 16_384,
    max_entry_bytes_per_slot: 10_485_760,
    partitioned_epoch_rewards_stake_account_stores_per_block: 2_048,
};

/// Slot-time reduction gates in the intended activation order.
const SLOT_TIME_REDUCTION_PARAMS: [(Pubkey, SlotParams); 4] = [
    (
        feature_set::reduce_slot_time_to_350ms::ID,
        SLOT_PARAMS_350MS,
    ),
    (
        feature_set::reduce_slot_time_to_300ms::ID,
        SLOT_PARAMS_300MS,
    ),
    (
        feature_set::reduce_slot_time_to_250ms::ID,
        SLOT_PARAMS_250MS,
    ),
    (
        feature_set::reduce_slot_time_to_200ms::ID,
        SLOT_PARAMS_200MS,
    ),
];

/// Returns slot-time feature gates mapped to runtime slot parameters.
pub fn slot_time_feature_gates() -> [(Pubkey, SlotParams); 4] {
    SLOT_TIME_REDUCTION_PARAMS
}

/// Returns all slot-time reduction feature IDs in activation order.
pub fn slot_time_feature_ids() -> [Pubkey; 4] {
    SLOT_TIME_REDUCTION_PARAMS.map(|(feature_id, _)| feature_id)
}

/// Bank-local archive of slot-parameter transitions.
///
/// This is rebuilt from feature accounts on startup instead of being serialized
/// into snapshots. The source of truth remains the feature set; this cache only
/// avoids repeatedly scanning, collecting, and sorting slot-time gates once they
/// are added.
#[derive(Clone, Debug)]
pub(crate) struct SlotParamsArchive {
    /// Sorted `(effective_slot, params)` transitions, including baseline at slot 0.
    ///
    /// This is used when a bank, usually the root bank, must answer "which
    /// parameters apply to some other slot?" Examples include shred filtering
    /// for incoming shreds and inflation calculations that span historical slot
    /// ranges. The transitions are normalized so slot duration never increases
    /// as slots advance, even if slot-time features activate out of order.
    param_transitions: BTreeMap<Slot, SlotParams>,
}

impl Default for SlotParamsArchive {
    fn default() -> Self {
        Self::new(
            &FeatureSet::default(),
            &EpochSchedule::default(),
            LEGACY_SLOT_PARAMS,
        )
    }
}

impl SlotParamsArchive {
    /// Rebuilds slot-parameter transitions from the active feature set.
    pub(crate) fn new(
        feature_set: &FeatureSet,
        epoch_schedule: &EpochSchedule,
        baseline_params: SlotParams,
    ) -> Self {
        let mut param_transitions = BTreeMap::from([(0, baseline_params)]);
        let mut earliest_same_or_shorter_slot = Slot::MAX;

        // The feature table is ordered longest-to-shortest. Walk it in reverse
        // so once a same-or-shorter target is effective at slot S, any longer
        // target effective at S or later is known to be redundant.
        for (feature_id, params) in slot_time_feature_gates().into_iter().rev() {
            if params.ns_per_slot > baseline_params.ns_per_slot {
                continue;
            }
            let Some(activation_slot) = feature_set.activated_slot(&feature_id) else {
                continue;
            };
            let effective_slot = Self::feature_effective_slot(epoch_schedule, activation_slot);
            if effective_slot < earliest_same_or_shorter_slot {
                param_transitions.insert(effective_slot, params);
                earliest_same_or_shorter_slot = effective_slot;
            }
        }

        Self { param_transitions }
    }

    /// Returns the baseline params supplied at genesis or snapshot restore.
    pub(crate) fn baseline_params(&self) -> SlotParams {
        self.param_transitions
            .first_key_value()
            .map(|(_, params)| *params)
            .unwrap_or(LEGACY_SLOT_PARAMS)
    }

    /// Returns the slot params effective at `slot`.
    pub(crate) fn params_at_slot(&self, slot: Slot) -> SlotParams {
        self.param_transitions
            .range(..=slot)
            .next_back()
            .map(|(_, params)| *params)
            .unwrap_or(LEGACY_SLOT_PARAMS)
    }

    /// Returns sorted slot-parameter transitions.
    pub(crate) fn param_transitions(&self) -> impl Iterator<Item = (Slot, SlotParams)> + '_ {
        self.param_transitions
            .iter()
            .map(|(slot, params)| (*slot, *params))
    }

    /// Returns the exact wall-clock duration in nanoseconds for
    /// `start_slot..=end_slot`.
    pub(crate) fn slot_range_duration_nanos(&self, start_slot: Slot, end_slot: Slot) -> u128 {
        if start_slot > end_slot {
            return 0;
        }

        let mut cursor = start_slot;
        let mut params = self.params_at_slot(start_slot);
        let mut duration = 0u128;

        for (&effective_slot, &effective_params) in self
            .param_transitions
            .range((Excluded(start_slot), Included(end_slot)))
        {
            duration = duration.saturating_add(
                u128::from(effective_slot.saturating_sub(cursor))
                    .saturating_mul(params.ns_per_slot()),
            );
            cursor = effective_slot;
            params = effective_params;
        }

        let remaining_slots = u128::from(end_slot.saturating_sub(cursor)) + 1;
        duration.saturating_add(remaining_slots.saturating_mul(params.ns_per_slot()))
    }

    /// Returns the first slot where a slot-time feature may affect bank state.
    ///
    /// A gate that activates in epoch E is effective starting at the first slot
    /// of epoch E + 1, giving shred filters a full epoch of advance notice
    /// before enforcing lower shred limits.
    fn feature_effective_slot(epoch_schedule: &EpochSchedule, activation_slot: Slot) -> Slot {
        let activation_epoch = epoch_schedule.get_epoch(activation_slot);
        epoch_schedule.get_first_slot_in_epoch(activation_epoch.saturating_add(1))
    }

    /// Returns true if any slot-time reduction has taken effect by this bank.
    ///
    /// Feature activation happens in one epoch, but slot params become effective
    /// at the start of the following epoch.
    pub(crate) fn any_slot_time_reduction_effective(
        epoch_schedule: &EpochSchedule,
        slot: Slot,
        feature_set: &FeatureSet,
        ns_per_slot: u128,
    ) -> bool {
        slot_time_feature_gates()
            .into_iter()
            .filter(|(_, params)| params.ns_per_slot() <= ns_per_slot)
            .filter_map(|(feature_id, _)| feature_set.activated_slot(&feature_id))
            .any(|activation_slot| {
                Self::feature_effective_slot(epoch_schedule, activation_slot) <= slot
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cost_limits_scaling_matches_simd_0525() {
        for (params, expected_account_limit, expected_block_limit) in [
            (LEGACY_SLOT_PARAMS, 40_000_000, 100_000_000),
            (SLOT_PARAMS_350MS, 35_000_000, 87_500_000),
            (SLOT_PARAMS_300MS, 30_000_000, 75_000_000),
            (SLOT_PARAMS_250MS, 25_000_000, 62_500_000),
            (SLOT_PARAMS_200MS, 20_000_000, 50_000_000),
        ] {
            let data_size_limit = params.cost_limits(false).allocated_data_size;
            assert_eq!(
                params.cost_limits(true),
                CostTrackerLimits::new(
                    expected_account_limit,
                    expected_block_limit,
                    data_size_limit
                )
            );
        }
    }
}
