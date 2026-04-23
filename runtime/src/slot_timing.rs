use {
    agave_feature_set::{self as feature_set, FeatureSet},
    solana_clock::{Epoch, Slot},
    solana_epoch_schedule::EpochSchedule,
    solana_pubkey::Pubkey,
};

/// Slot timing parameters for one SIMD-0469 stage.
///
/// Slot duration and epoch length are stored as exact rational multiples of the
/// pre-transition schedule instead of as absolute milliseconds/nanoseconds.
/// That keeps stages such as 266.666...ms (2/3 of the base slot) and
/// 228.571...ms (4/7 of the base slot) exact, and it lets tests with custom
/// genesis slot durations inherit the same proportional changes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SlotTimingConfig {
    /// Numerator of the multiplier applied to the base slot duration.
    pub slot_time_numerator: u64,
    /// Denominator of the multiplier applied to the base slot duration.
    pub slot_time_denominator: u64,
    /// Numerator of the multiplier applied to the base epoch length.
    pub slots_per_epoch_numerator: u64,
    /// Denominator of the multiplier applied to the base epoch length.
    pub slots_per_epoch_denominator: u64,
    /// Number of consecutive slots assigned to the same leader.
    pub num_consecutive_leader_slots: u64,
}

/// A contiguous epoch range that shares one slot-timing configuration.
///
/// The embedded `epoch_schedule` is fully adjusted so callers can use it with
/// absolute slots and epochs directly, even after one or more staged timing
/// transitions have occurred.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotTimingEpochSegment {
    /// First epoch for which `epoch_schedule` and `slot_timing_config` apply.
    pub first_epoch: Epoch,
    /// First slot for which `epoch_schedule` and `slot_timing_config` apply.
    pub first_slot: Slot,
    /// Epoch schedule valid for epochs and slots in this segment.
    pub epoch_schedule: EpochSchedule,
    /// Timing configuration represented by this segment.
    pub slot_timing_config: SlotTimingConfig,
}

/// Slot timing before any SIMD-0469 transition is active.
pub const DEFAULT_SLOT_TIMING_CONFIG: SlotTimingConfig = SlotTimingConfig {
    slot_time_numerator: 1,
    slot_time_denominator: 1,
    slots_per_epoch_numerator: 1,
    slots_per_epoch_denominator: 1,
    num_consecutive_leader_slots: 4,
};

/// Ordered SIMD-0469 slot timing feature gates.
///
/// The order is consensus-significant when multiple gates are active: the last
/// activated transition in this list wins. Bank code applies non-genesis
/// activations with an epoch delay so already-generated leader schedules are not
/// reinterpreted with new leader-window parameters.
pub const SLOT_TIMING_TRANSITIONS: [(Pubkey, SlotTimingConfig); 10] = [
    (
        feature_set::slot_time_320ms_5_slot_span::ID,
        SlotTimingConfig {
            slot_time_numerator: 4,
            slot_time_denominator: 5,
            slots_per_epoch_numerator: 5,
            slots_per_epoch_denominator: 4,
            num_consecutive_leader_slots: 5,
        },
    ),
    (
        feature_set::slot_time_266ms_6_slot_span::ID,
        SlotTimingConfig {
            slot_time_numerator: 2,
            slot_time_denominator: 3,
            slots_per_epoch_numerator: 3,
            slots_per_epoch_denominator: 2,
            num_consecutive_leader_slots: 6,
        },
    ),
    (
        feature_set::slot_time_229ms_7_slot_span::ID,
        SlotTimingConfig {
            slot_time_numerator: 4,
            slot_time_denominator: 7,
            slots_per_epoch_numerator: 7,
            slots_per_epoch_denominator: 4,
            num_consecutive_leader_slots: 7,
        },
    ),
    (
        feature_set::slot_time_200ms_8_slot_span::ID,
        SlotTimingConfig {
            slot_time_numerator: 1,
            slot_time_denominator: 2,
            slots_per_epoch_numerator: 2,
            slots_per_epoch_denominator: 1,
            num_consecutive_leader_slots: 8,
        },
    ),
    (
        feature_set::slot_time_200ms_7_slot_span_adjusted_epoch::ID,
        SlotTimingConfig {
            slot_time_numerator: 1,
            slot_time_denominator: 2,
            slots_per_epoch_numerator: 215_999,
            slots_per_epoch_denominator: 108_000,
            num_consecutive_leader_slots: 7,
        },
    ),
    (
        feature_set::slot_time_200ms_6_slot_span::ID,
        SlotTimingConfig {
            slot_time_numerator: 1,
            slot_time_denominator: 2,
            slots_per_epoch_numerator: 2,
            slots_per_epoch_denominator: 1,
            num_consecutive_leader_slots: 6,
        },
    ),
    (
        feature_set::slot_time_200ms_5_slot_span::ID,
        SlotTimingConfig {
            slot_time_numerator: 1,
            slot_time_denominator: 2,
            slots_per_epoch_numerator: 2,
            slots_per_epoch_denominator: 1,
            num_consecutive_leader_slots: 5,
        },
    ),
    (
        feature_set::slot_time_200ms_4_slot_span::ID,
        SlotTimingConfig {
            slot_time_numerator: 1,
            slot_time_denominator: 2,
            slots_per_epoch_numerator: 2,
            slots_per_epoch_denominator: 1,
            num_consecutive_leader_slots: 4,
        },
    ),
    (
        feature_set::slot_time_200ms_3_slot_span::ID,
        SlotTimingConfig {
            slot_time_numerator: 1,
            slot_time_denominator: 2,
            slots_per_epoch_numerator: 2,
            slots_per_epoch_denominator: 1,
            num_consecutive_leader_slots: 3,
        },
    ),
    (
        feature_set::slot_time_200ms_2_slot_span::ID,
        SlotTimingConfig {
            slot_time_numerator: 1,
            slot_time_denominator: 2,
            slots_per_epoch_numerator: 2,
            slots_per_epoch_denominator: 1,
            num_consecutive_leader_slots: 2,
        },
    ),
];

/// Return the latest activated slot timing config, ignoring bank-level delay.
///
/// This is appropriate for genesis-only calculations. Runtime code that needs
/// the effective timing for a bank or slot should ask `Bank`, because the bank
/// applies non-genesis transitions one epoch after feature activation.
pub fn slot_timing_config(feature_set: &FeatureSet) -> SlotTimingConfig {
    slot_timing_config_at_slot(feature_set, u64::MAX)
}

/// Return the latest slot timing config activated at or before `slot`.
///
/// This helper intentionally only knows feature activation slots. It does not
/// apply the bank-level one-epoch delay for live transitions.
pub fn slot_timing_config_at_slot(feature_set: &FeatureSet, slot: u64) -> SlotTimingConfig {
    SLOT_TIMING_TRANSITIONS
        .iter()
        .filter(|(feature_id, _)| {
            feature_set
                .activated_slot(feature_id)
                .is_some_and(|s| s <= slot)
        })
        .map(|(_, config)| *config)
        .next_back()
        .unwrap_or(DEFAULT_SLOT_TIMING_CONFIG)
}

/// Return slots/epoch for `slot_timing_config` from `base_epoch_schedule`.
pub fn slots_per_epoch_for_slot_timing_config(
    base_epoch_schedule: &EpochSchedule,
    slot_timing_config: SlotTimingConfig,
) -> u64 {
    mul_div_u64_round(
        base_epoch_schedule.slots_per_epoch,
        slot_timing_config.slots_per_epoch_numerator,
        slot_timing_config.slots_per_epoch_denominator,
    )
}

/// Return the leader-schedule offset for `slot_timing_config` from `base_epoch_schedule`.
pub fn leader_schedule_slot_offset_for_slot_timing_config(
    base_epoch_schedule: &EpochSchedule,
    slot_timing_config: SlotTimingConfig,
) -> u64 {
    mul_div_u64_round(
        base_epoch_schedule.leader_schedule_slot_offset,
        slot_timing_config.slots_per_epoch_numerator,
        slot_timing_config.slots_per_epoch_denominator,
    )
}

/// Return the first slot of `epoch` within `segment`.
pub fn slot_timing_segment_first_slot_in_epoch(
    segment: &SlotTimingEpochSegment,
    epoch: Epoch,
) -> Slot {
    if epoch == segment.first_epoch {
        segment.first_slot
    } else {
        segment.epoch_schedule.get_first_slot_in_epoch(epoch)
    }
}

/// Build the epoch-schedule segments implied by slot timing feature activations.
///
/// Features active at genesis take effect immediately. Features activated later
/// take effect at the next epoch boundary so the leader schedule generated one
/// epoch ahead is never reinterpreted with different slot-span parameters.
pub fn slot_timing_epoch_segments(
    feature_set: &FeatureSet,
    base_epoch_schedule: &EpochSchedule,
) -> Vec<SlotTimingEpochSegment> {
    let mut segments = vec![SlotTimingEpochSegment {
        first_epoch: 0,
        first_slot: 0,
        epoch_schedule: base_epoch_schedule.clone(),
        slot_timing_config: DEFAULT_SLOT_TIMING_CONFIG,
    }];

    for (feature_id, slot_timing_config) in SLOT_TIMING_TRANSITIONS.iter() {
        let Some(activation_slot) = feature_set.activated_slot(feature_id) else {
            continue;
        };
        let activation_segment = segments
            .iter()
            .rev()
            .find(|segment| segment.first_slot <= activation_slot)
            .expect("slot timing segments must always contain the base segment");
        let activation_epoch = activation_segment.epoch_schedule.get_epoch(activation_slot);
        let first_epoch = if activation_slot == 0 {
            activation_epoch
        } else {
            activation_epoch.saturating_add(1)
        };
        let first_slot = slot_timing_segment_first_slot_in_epoch(activation_segment, first_epoch);
        let epoch_schedule = EpochSchedule {
            slots_per_epoch: slots_per_epoch_for_slot_timing_config(
                base_epoch_schedule,
                *slot_timing_config,
            ),
            leader_schedule_slot_offset: leader_schedule_slot_offset_for_slot_timing_config(
                base_epoch_schedule,
                *slot_timing_config,
            ),
            warmup: false,
            first_normal_epoch: first_epoch,
            first_normal_slot: first_slot,
        };

        if let Some(segment) = segments
            .iter_mut()
            .find(|segment| segment.first_slot == first_slot)
        {
            segment.first_epoch = first_epoch;
            segment.epoch_schedule = epoch_schedule;
            segment.slot_timing_config = *slot_timing_config;
            continue;
        }

        segments.push(SlotTimingEpochSegment {
            first_epoch,
            first_slot,
            epoch_schedule,
            slot_timing_config: *slot_timing_config,
        });
        segments.sort_by_key(|segment| segment.first_slot);
    }
    segments
}

/// Return the slot-timing segment that applies to `epoch`.
pub fn slot_timing_epoch_segment_for_epoch(
    segments: &[SlotTimingEpochSegment],
    epoch: Epoch,
) -> Option<&SlotTimingEpochSegment> {
    segments
        .iter()
        .rev()
        .find(|segment| segment.first_epoch <= epoch)
}

/// Return the slot-timing segment that applies to `slot`.
pub fn slot_timing_epoch_segment_for_slot(
    segments: &[SlotTimingEpochSegment],
    slot: Slot,
) -> Option<&SlotTimingEpochSegment> {
    segments
        .iter()
        .rev()
        .find(|segment| segment.first_slot <= slot)
}

/// Saturating `(value * numerator) / denominator`, rounded to nearest.
fn mul_div_u64_round(value: u64, numerator: u64, denominator: u64) -> u64 {
    let denominator = denominator.max(1);
    value
        .saturating_mul(numerator)
        .saturating_add(denominator / 2)
        .saturating_div(denominator)
}
