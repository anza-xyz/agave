use {
    arc_swap::ArcSwapOption,
    solana_clock::Slot,
    solana_runtime::leader_schedule_utils::{
        first_of_consecutive_leader_slots, last_of_consecutive_leader_slots,
    },
    std::{
        sync::Arc,
        time::{Duration, Instant},
    },
};

/// The latest Alpenglow leader-window start observed on this validator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AlpenglowSlotInfo {
    /// The first slot in the leader window.
    pub slot: Slot,
    /// When the slot start was observed locally.
    pub started_at: Instant,
    /// The effective duration for this slot.
    pub slot_duration: Duration,
}

/// Lock-free shared access to the latest observed Alpenglow leader window.
#[derive(Clone, Default)]
pub struct SharedAlpenglowSlotClock(Arc<ArcSwapOption<AlpenglowSlotInfo>>);

impl SharedAlpenglowSlotClock {
    /// Updates the latest observed leader window.
    ///
    /// Duplicate, out-of-order, or recovery events within an already observed
    /// window must not restart progress, even if vote history was pruned.
    pub fn update(&self, slot: Slot, started_at: Instant, slot_duration: Duration) {
        let slot_info = Arc::new(AlpenglowSlotInfo {
            slot,
            started_at,
            slot_duration,
        });
        self.0.rcu(|current| {
            if current.as_ref().is_some_and(|current| {
                first_of_consecutive_leader_slots(current.slot)
                    >= first_of_consecutive_leader_slots(slot)
            }) {
                current.clone()
            } else {
                Some(slot_info.clone())
            }
        });
    }

    /// Updates the clock for a leader window, replacing an existing observation
    /// of the same window when block production restarts with a new parent.
    pub fn update_leader(&self, slot: Slot, started_at: Instant, slot_duration: Duration) {
        let slot_info = Arc::new(AlpenglowSlotInfo {
            slot,
            started_at,
            slot_duration,
        });
        self.0.rcu(|current| {
            if current.as_ref().is_some_and(|current| current.slot > slot) {
                current.clone()
            } else {
                Some(slot_info.clone())
            }
        });
    }

    /// Loads the latest observed leader window.
    pub fn load(&self) -> Option<AlpenglowSlotInfo> {
        self.0.load().as_ref().map(|slot_info| **slot_info)
    }

    /// Estimates the slot currently in progress within the latest observed
    /// Alpenglow leader window.
    pub fn current_slot(&self) -> Option<Slot> {
        self.current_slot_at(Instant::now())
    }

    fn current_slot_at(&self, now: Instant) -> Option<Slot> {
        let slot_info = self.load()?;
        let last_slot = last_of_consecutive_leader_slots(slot_info.slot);
        if slot_info.slot_duration.is_zero() {
            return Some(last_slot);
        }
        let elapsed_slots = now
            .saturating_duration_since(slot_info.started_at)
            .as_nanos()
            .saturating_div(slot_info.slot_duration.as_nanos());
        let elapsed_slots = Slot::try_from(elapsed_slots).unwrap_or(Slot::MAX);
        Some(slot_info.slot.saturating_add(elapsed_slots).min(last_slot))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shared_alpenglow_slot_clock() {
        let clock = SharedAlpenglowSlotClock::default();
        let started_at = Instant::now();
        let slot_duration = Duration::from_millis(400);

        assert_eq!(clock.load(), None);

        clock.update(4, started_at, slot_duration);
        let slot_info = AlpenglowSlotInfo {
            slot: 4,
            started_at,
            slot_duration,
        };
        assert_eq!(clock.load(), Some(slot_info));

        clock.update(
            4,
            started_at + Duration::from_millis(1),
            Duration::from_millis(200),
        );
        clock.update(3, started_at, slot_duration);
        assert_eq!(clock.load(), Some(slot_info));

        // Recovery for a later slot in the same window must keep the anchor.
        clock.update(5, started_at + slot_duration, Duration::from_millis(200));
        clock.update(7, started_at + slot_duration * 3, slot_duration);
        assert_eq!(clock.load(), Some(slot_info));

        let next_started_at = started_at + slot_duration * 4;
        clock.update(8, next_started_at, Duration::from_millis(200));
        assert_eq!(
            clock.load(),
            Some(AlpenglowSlotInfo {
                slot: 8,
                started_at: next_started_at,
                slot_duration: Duration::from_millis(200),
            })
        );

        let replacement_started_at = next_started_at + Duration::from_millis(2);
        clock.update_leader(8, replacement_started_at, Duration::from_millis(200));
        assert_eq!(
            clock.load(),
            Some(AlpenglowSlotInfo {
                slot: 8,
                started_at: replacement_started_at,
                slot_duration: Duration::from_millis(200),
            })
        );

        clock.update_leader(4, started_at, slot_duration);
        assert_eq!(clock.load().unwrap().slot, 8);
    }

    #[test]
    fn test_shared_alpenglow_slot_clock_mid_window_recovery() {
        let clock = SharedAlpenglowSlotClock::default();
        let started_at = Instant::now();
        let slot_duration = Duration::from_millis(200);

        // A previously unobserved window can first be anchored by recovery.
        clock.update(5, started_at, slot_duration);
        let slot_info = clock.load().unwrap();
        assert_eq!(slot_info.slot, 5);
        clock.update(6, started_at + slot_duration, slot_duration);
        clock.update(4, started_at + slot_duration, slot_duration);
        assert_eq!(clock.load(), Some(slot_info));

        // Local production may still replace the timer on a parent switch.
        let replacement_started_at = started_at + slot_duration;
        clock.update_leader(5, replacement_started_at, slot_duration);
        assert_eq!(clock.load().unwrap().started_at, replacement_started_at);

        clock.update(8, replacement_started_at + slot_duration, slot_duration);
        assert_eq!(clock.load().unwrap().slot, 8);
    }

    #[test]
    fn test_current_slot() {
        let clock = SharedAlpenglowSlotClock::default();
        let started_at = Instant::now();
        let slot_duration = Duration::from_millis(200);
        assert_eq!(clock.current_slot_at(started_at), None);

        clock.update(4, started_at, slot_duration);
        assert_eq!(clock.current_slot_at(started_at), Some(4));
        assert_eq!(
            clock.current_slot_at(started_at + slot_duration * 2),
            Some(6)
        );
        assert_eq!(
            clock.current_slot_at(started_at + Duration::from_secs(10)),
            Some(7)
        );
    }
}
