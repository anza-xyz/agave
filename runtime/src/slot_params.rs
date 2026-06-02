use {
    solana_clock::Slot,
    solana_epoch_schedule::EpochSchedule,
    std::{
        collections::BTreeMap,
        ops::Bound::{Excluded, Included},
    },
};

pub const DEFAULT_MAX_ENTRY_BYTES_PER_SLOT: u64 = 20 * 1024 * 1024; // 20 MiB

/// Runtime parameters tied to a slot-time regime.
///
/// This intentionally groups values that need to move together when slot time
/// changes. For now, only the legacy baseline exists, so routing through this
/// type preserves current behavior while giving future feature-gated changes a
/// single table-shaped home.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SlotParams {
    pub(crate) ns_per_slot: u128,
    pub(crate) slots_per_year: f64,
    pub(crate) hashes_per_tick: Option<u64>,
    pub(crate) max_block_units: u64,
    pub(crate) max_writable_account_units: u64,
    pub(crate) max_vote_units: u64,
    pub(crate) max_block_accounts_data_size_delta: u64,
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
    pub const fn cost_limits(self) -> (u64, u64, u64, u64) {
        (
            self.max_writable_account_units,
            self.max_block_units,
            self.max_vote_units,
            self.max_block_accounts_data_size_delta,
        )
    }
}

pub(crate) const LEGACY_SLOT_PARAMS: SlotParams = SlotParams {
    ns_per_slot: 400_000_000,
    slots_per_year: 78_892_314.984,
    hashes_per_tick: Some(62_500),
    max_block_units: 60_000_000,
    max_writable_account_units: 24_000_000,
    max_vote_units: 36_000_000,
    max_block_accounts_data_size_delta: 100_000_000,
    max_data_shreds_per_slot: 32_768,
    max_code_shreds_per_slot: 32_768,
    max_entry_bytes_per_slot: 20 * 1024 * 1024,
    partitioned_epoch_rewards_stake_account_stores_per_block: 4096,
};

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
    /// ranges.
    param_transitions: BTreeMap<Slot, SlotParams>,
}

impl Default for SlotParamsArchive {
    fn default() -> Self {
        Self::new(&EpochSchedule::default(), LEGACY_SLOT_PARAMS)
    }
}

impl SlotParamsArchive {
    /// Rebuilds slot-parameter transitions from the supplied baseline.
    ///
    /// `_epoch_schedule` is accepted now so the call sites already have the
    /// right shape for delayed, epoch-boundary-effective feature transitions.
    pub(crate) fn new(_epoch_schedule: &EpochSchedule, baseline_params: SlotParams) -> Self {
        Self {
            param_transitions: BTreeMap::from([(0, baseline_params)]),
        }
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
}
