//! This module provides [`RecentLeaderSlots`] to track recent leader slots.
use {crate::node_address_service::SlotEvent, solana_clock::Slot, std::collections::VecDeque};

// 48 chosen because it's unlikely that 12 leaders in a row will miss their slots
const MAX_SLOT_SKIP_DISTANCE: u64 = 48;

const RECENT_LEADER_SLOTS_CAPACITY: usize = 48;

#[derive(Debug)]
pub struct RecentLeaderSlots(VecDeque<SlotEvent>);

impl RecentLeaderSlots {
    pub fn new() -> Self {
        Self(VecDeque::with_capacity(RECENT_LEADER_SLOTS_CAPACITY))
    }
}

impl Default for RecentLeaderSlots {
    fn default() -> Self {
        Self::new()
    }
}

impl RecentLeaderSlots {
    pub fn record(&mut self, slot_event: SlotEvent) {
        while self.0.len() > RECENT_LEADER_SLOTS_CAPACITY.saturating_sub(1) {
            self.0.pop_front();
        }
        self.0.push_back(slot_event);
    }

    // Estimate the current slot from recent slot notifications.
    #[allow(clippy::arithmetic_side_effects)]
    pub fn estimate_current_slot(&self) -> Slot {
        let mut recent_slots: Vec<SlotEvent> = self.0.iter().cloned().collect();
        assert!(
            !recent_slots.is_empty(),
            "method must be called after at least one record."
        );
        recent_slots.sort_by(|a, b| {
            a.slot()
                .cmp(&b.slot())
                .then_with(|| b.is_start().cmp(&a.is_start())) // true before false
        });

        // Validators can broadcast invalid blocks that are far in the future so check if the
        // current slot is in line with the recent progression.
        let max_index = recent_slots.len() - 1;
        let median_index = max_index / 2;
        let median_recent_slot = recent_slots[median_index].slot();
        let expected_current_slot = median_recent_slot + (max_index - median_index) as u64;
        let max_reasonable_current_slot = expected_current_slot + MAX_SLOT_SKIP_DISTANCE;

        let idx = recent_slots
            .iter()
            .rposition(|e| e.slot() <= max_reasonable_current_slot)
            .expect("no reasonable slot");

        let slot_event = &recent_slots[idx];
        if slot_event.is_start() {
            slot_event.slot()
        } else {
            slot_event.slot().saturating_add(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_clock::Slot};

    impl From<Vec<Slot>> for RecentLeaderSlots {
        fn from(recent_slots: Vec<Slot>) -> Self {
            use std::collections::VecDeque;
            assert!(!recent_slots.is_empty());

            let mut events = VecDeque::with_capacity(recent_slots.len());

            for slot in recent_slots {
                events.push_back(SlotEvent::Start(slot));
                events.push_back(SlotEvent::End(slot));
            }

            Self(events)
        }
    }

    #[test]
    fn test_recent_leader_slots() {
        let mut recent_slots: Vec<Slot> = (1..=12).collect();
        assert_eq!(
            RecentLeaderSlots::from(recent_slots.clone()).estimate_current_slot(),
            13
        );

        recent_slots.reverse();
        assert_eq!(
            RecentLeaderSlots::from(recent_slots).estimate_current_slot(),
            13
        );

        let mut recent_slots = RecentLeaderSlots::new();
        recent_slots.record(SlotEvent::Start(13));
        assert_eq!(recent_slots.estimate_current_slot(), 13);
        recent_slots.record(SlotEvent::Start(14));
        assert_eq!(recent_slots.estimate_current_slot(), 14);
        recent_slots.record(SlotEvent::Start(15));
        assert_eq!(recent_slots.estimate_current_slot(), 15);

        assert_eq!(
            RecentLeaderSlots::from(vec![0, 1 + MAX_SLOT_SKIP_DISTANCE]).estimate_current_slot(),
            2 + MAX_SLOT_SKIP_DISTANCE,
        );
        assert_eq!(
            RecentLeaderSlots::from(vec![0, 2 + MAX_SLOT_SKIP_DISTANCE]).estimate_current_slot(),
            3 + MAX_SLOT_SKIP_DISTANCE,
        );

        assert_eq!(
            RecentLeaderSlots::from(vec![1, 100]).estimate_current_slot(),
            2
        );
        assert_eq!(
            RecentLeaderSlots::from(vec![1, 2, 100]).estimate_current_slot(),
            3
        );
        assert_eq!(
            RecentLeaderSlots::from(vec![1, 2, 3, 100]).estimate_current_slot(),
            4
        );
        assert_eq!(
            RecentLeaderSlots::from(vec![1, 2, 3, 99, 100]).estimate_current_slot(),
            4
        );
    }
}
