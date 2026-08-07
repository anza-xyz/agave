use {
    arc_swap::ArcSwapOption,
    solana_clock::Slot,
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
    /// Duplicate or out-of-order events must not restart progress for the
    /// current window.
    pub fn update(&self, slot: Slot, started_at: Instant, slot_duration: Duration) {
        let slot_info = Arc::new(AlpenglowSlotInfo {
            slot,
            started_at,
            slot_duration,
        });
        self.0.rcu(|current| {
            if current.as_ref().is_some_and(|current| current.slot >= slot) {
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

        let next_started_at = started_at + slot_duration;
        clock.update(5, next_started_at, Duration::from_millis(200));
        assert_eq!(
            clock.load(),
            Some(AlpenglowSlotInfo {
                slot: 5,
                started_at: next_started_at,
                slot_duration: Duration::from_millis(200),
            })
        );

        let replacement_started_at = next_started_at + Duration::from_millis(2);
        clock.update_leader(5, replacement_started_at, Duration::from_millis(200));
        assert_eq!(
            clock.load(),
            Some(AlpenglowSlotInfo {
                slot: 5,
                started_at: replacement_started_at,
                slot_duration: Duration::from_millis(200),
            })
        );

        clock.update_leader(4, started_at, slot_duration);
        assert_eq!(clock.load().unwrap().slot, 5);
    }
}
