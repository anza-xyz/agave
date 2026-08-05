use {
    arc_swap::ArcSwapOption,
    solana_clock::Slot,
    std::{
        sync::Arc,
        time::{Duration, Instant},
    },
};

/// The latest Alpenglow leader-window start observed by Votor on this validator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AlpenglowSlotInfo {
    /// The first slot in the leader window.
    pub slot: Slot,
    /// When the slot start was observed locally.
    pub started_at: Instant,
    /// The effective duration for this slot.
    pub slot_duration: Duration,
}

/// Lock-free shared access to Votor's latest observed Alpenglow leader window.
#[derive(Clone, Default)]
pub struct SharedAlpenglowSlotClock(Arc<ArcSwapOption<AlpenglowSlotInfo>>);

impl SharedAlpenglowSlotClock {
    /// Updates the latest observed leader window.
    ///
    /// Votor's event handler is the sole writer. Duplicate or out-of-order
    /// events must not restart progress for the current window.
    pub fn update(&self, slot: Slot, started_at: Instant, slot_duration: Duration) {
        let current = self.0.load();
        if current.as_ref().is_some_and(|current| current.slot >= slot) {
            return;
        }
        drop(current);
        self.0.store(Some(Arc::new(AlpenglowSlotInfo {
            slot,
            started_at,
            slot_duration,
        })));
    }

    /// Loads the latest observed leader window, if Votor has reported one.
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
    }
}
