//! Shared slot timing calculations for Alpenglow.

use {
    solana_clock::Slot, solana_runtime::leader_schedule_utils::last_of_consecutive_leader_slots,
    std::time::Duration,
};

pub(super) const ALPENGLOW_PROGRESS_STEP: u8 = 5;

fn alpenglow_progress(elapsed: Duration, slot_duration: Duration) -> u8 {
    if slot_duration.is_zero() {
        return 100;
    }

    let percentage = elapsed
        .as_nanos()
        .saturating_mul(100)
        .saturating_div(slot_duration.as_nanos())
        .min(100) as u8;
    (percentage / ALPENGLOW_PROGRESS_STEP) * ALPENGLOW_PROGRESS_STEP
}

pub(super) fn alpenglow_slot_progress(
    window_start_slot: Slot,
    elapsed: Duration,
    slot_duration: Duration,
) -> (Slot, u8) {
    let window_end_slot = last_of_consecutive_leader_slots(window_start_slot);
    if slot_duration.is_zero() {
        return (window_end_slot, 100);
    }

    let elapsed_nanos = elapsed.as_nanos();
    let slot_duration_nanos = slot_duration.as_nanos();
    let elapsed_slots = elapsed_nanos / slot_duration_nanos;
    let window_slot_offset = u128::from(window_end_slot - window_start_slot);
    if elapsed_slots > window_slot_offset {
        return (window_end_slot, 100);
    }

    let current_slot = window_start_slot + elapsed_slots as Slot;
    let elapsed_in_slot = Duration::from_nanos_u128(elapsed_nanos % slot_duration_nanos);
    (
        current_slot,
        alpenglow_progress(elapsed_in_slot, slot_duration),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alpenglow_progress() {
        let slot_duration = Duration::from_millis(400);
        assert_eq!(alpenglow_progress(Duration::ZERO, slot_duration), 0);
        assert_eq!(
            alpenglow_progress(Duration::from_millis(19), slot_duration),
            0
        );
        assert_eq!(
            alpenglow_progress(Duration::from_millis(20), slot_duration),
            5
        );
        assert_eq!(
            alpenglow_progress(Duration::from_millis(399), slot_duration),
            95
        );
        assert_eq!(alpenglow_progress(slot_duration, slot_duration), 100);
        assert_eq!(
            alpenglow_progress(Duration::from_millis(800), slot_duration),
            100
        );
        assert_eq!(alpenglow_progress(Duration::ZERO, Duration::ZERO), 100);
    }

    #[test]
    fn test_alpenglow_slot_progress() {
        let slot_duration = Duration::from_millis(400);
        assert_eq!(
            alpenglow_slot_progress(4, Duration::ZERO, slot_duration),
            (4, 0)
        );
        assert_eq!(
            alpenglow_slot_progress(4, Duration::from_millis(399), slot_duration),
            (4, 95)
        );
        assert_eq!(
            alpenglow_slot_progress(4, slot_duration, slot_duration),
            (5, 0)
        );
        assert_eq!(
            alpenglow_slot_progress(4, Duration::from_millis(800), slot_duration),
            (6, 0)
        );
        assert_eq!(
            alpenglow_slot_progress(4, Duration::from_millis(1_599), slot_duration),
            (7, 95)
        );
        assert_eq!(
            alpenglow_slot_progress(4, Duration::from_millis(1_600), slot_duration),
            (7, 100)
        );
        assert_eq!(
            alpenglow_slot_progress(4, Duration::from_millis(200), Duration::from_millis(200)),
            (5, 0)
        );
        assert_eq!(
            alpenglow_slot_progress(4, Duration::ZERO, Duration::ZERO),
            (7, 100)
        );
    }
}
