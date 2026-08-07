use {
    arc_swap::ArcSwapOption,
    solana_clock::{BankId, Slot},
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
    /// The currently observed bank for this slot, if one has been created.
    pub bank_id: Option<BankId>,
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
            bank_id: None,
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

    /// Sets the clock, replacing an existing observation of the same window.
    /// Observations from older windows are ignored.
    pub fn set(&self, slot: Slot, started_at: Instant, slot_duration: Duration) {
        let slot_info = Arc::new(AlpenglowSlotInfo {
            slot,
            bank_id: None,
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

    /// Records the bank created for the current slot, restarting the clock if
    /// it replaces a previously observed bank.
    pub fn observe_bank(&self, slot: Slot, bank_id: BankId, observed_at: Instant) {
        self.0.rcu(|current| {
            let Some(slot_info) = current.as_ref().map(|slot_info| **slot_info) else {
                return current.clone();
            };
            if slot_info.slot != slot || slot_info.bank_id == Some(bank_id) {
                return current.clone();
            }
            Some(Arc::new(AlpenglowSlotInfo {
                bank_id: Some(bank_id),
                started_at: if slot_info.bank_id.is_some() {
                    observed_at
                } else {
                    slot_info.started_at
                },
                ..slot_info
            }))
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
            bank_id: None,
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
                bank_id: None,
                started_at: next_started_at,
                slot_duration: Duration::from_millis(200),
            })
        );

        let replacement_started_at = next_started_at + Duration::from_millis(2);
        clock.set(5, replacement_started_at, Duration::from_millis(200));
        assert_eq!(
            clock.load(),
            Some(AlpenglowSlotInfo {
                slot: 5,
                bank_id: None,
                started_at: replacement_started_at,
                slot_duration: Duration::from_millis(200),
            })
        );

        clock.set(4, started_at, slot_duration);
        assert_eq!(clock.load().unwrap().slot, 5);

        clock.observe_bank(5, 1, started_at);
        assert_eq!(
            clock.load().unwrap(),
            AlpenglowSlotInfo {
                slot: 5,
                bank_id: Some(1),
                started_at: replacement_started_at,
                slot_duration: Duration::from_millis(200),
            }
        );

        let bank_replaced_at = replacement_started_at + Duration::from_millis(2);
        clock.observe_bank(5, 2, bank_replaced_at);
        assert_eq!(clock.load().unwrap().bank_id, Some(2));
        assert_eq!(clock.load().unwrap().started_at, bank_replaced_at);
    }
}
