//! This module provides [`RecentLeaderSlots`] to track recent leader slots.
use {
    crate::{leader_updater::SlotEstimate, node_address_service::SlotEvent},
    solana_clock::Slot,
    solana_leader_schedule::NUM_CONSECUTIVE_LEADER_SLOTS,
    std::collections::VecDeque,
};

// 48 chosen because it's unlikely that 12 leaders in a row will miss their slots
const MAX_SLOT_SKIP_DISTANCE: u64 = 48;

const RECENT_LEADER_SLOTS_CAPACITY: usize = 48;

pub(crate) const MS_PER_SLOT: u64 = 350;

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

    /// Returns estimated current slot and the estimated end timestamp of its leader window.
    pub fn estimate_slot(&self) -> SlotEstimate {
        let (slot, estimated_duration_ms) = self.estimate_slot_and_duration_ms();
        let slot_duration = estimated_duration_ms.unwrap_or(MS_PER_SLOT);

        const SLOTS_PER_WINDOW: u64 = NUM_CONSECUTIVE_LEADER_SLOTS.get() as u64;
        let window_start_slot = slot.saturating_sub(slot % SLOTS_PER_WINDOW);

        let previous_slot = window_start_slot.checked_sub(1);
        let mut previous_slot_end_event = None;
        let mut window_start_event = None;
        for event in &self.0 {
            match event {
                SlotEvent::Start { slot, .. } if *slot == window_start_slot => {
                    window_start_event = Some(event);
                    break;
                }
                SlotEvent::End { slot, .. } if Some(*slot) == previous_slot => {
                    // Keep the most recently received matching end as the fallback.
                    previous_slot_end_event = Some(event);
                }
                _ => {}
            }
        }
        let window_start_event = window_start_event.or(previous_slot_end_event);

        let leader_window_duration = SLOTS_PER_WINDOW.checked_mul(slot_duration);
        let leader_window_end_ms = window_start_event
            .map(SlotEvent::timestamp)
            .zip(leader_window_duration)
            .and_then(|(start, duration)| start.checked_add(duration));

        SlotEstimate {
            slot,
            leader_window_end_ms,
        }
    }

    // Estimate the current slot and slot duration from recent slot notifications.
    #[allow(clippy::arithmetic_side_effects)]
    fn estimate_slot_and_duration_ms(&self) -> (Slot, Option<u64>) {
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
        let estimated_slot = if slot_event.is_start() {
            slot_event.slot()
        } else {
            slot_event.slot().saturating_add(1)
        };

        let mut start_events = recent_slots[..=idx].iter().filter(|event| event.is_start());
        let first = start_events.next();
        let last = start_events.next_back();
        let duration_ms = first.zip(last).and_then(|(first, last)| {
            let elapsed_ms = last.timestamp().checked_sub(first.timestamp())?;
            let elapsed_slots = last.slot().checked_sub(first.slot())?;
            let duration_ms = elapsed_ms.checked_div(elapsed_slots)?;
            (duration_ms > 0).then_some(duration_ms)
        });
        (estimated_slot, duration_ms)
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
                events.push_back(SlotEvent::Start { slot, timestamp: 0 });
                events.push_back(SlotEvent::End { slot, timestamp: 0 });
            }

            Self(events)
        }
    }

    #[test]
    fn test_recent_leader_slots() {
        let mut recent_slots: Vec<Slot> = (1..=12).collect();
        assert_eq!(
            RecentLeaderSlots::from(recent_slots.clone())
                .estimate_slot_and_duration_ms()
                .0,
            13
        );

        recent_slots.reverse();
        assert_eq!(
            RecentLeaderSlots::from(recent_slots)
                .estimate_slot_and_duration_ms()
                .0,
            13
        );

        let mut recent_slots = RecentLeaderSlots::new();
        recent_slots.record(SlotEvent::Start {
            slot: 13,
            timestamp: 0,
        });
        assert_eq!(recent_slots.estimate_slot_and_duration_ms().0, 13);
        recent_slots.record(SlotEvent::Start {
            slot: 14,
            timestamp: 0,
        });
        assert_eq!(recent_slots.estimate_slot_and_duration_ms().0, 14);
        recent_slots.record(SlotEvent::Start {
            slot: 15,
            timestamp: 0,
        });
        assert_eq!(recent_slots.estimate_slot_and_duration_ms().0, 15);

        assert_eq!(
            RecentLeaderSlots::from(vec![0, 1 + MAX_SLOT_SKIP_DISTANCE])
                .estimate_slot_and_duration_ms()
                .0,
            2 + MAX_SLOT_SKIP_DISTANCE,
        );
        assert_eq!(
            RecentLeaderSlots::from(vec![0, 2 + MAX_SLOT_SKIP_DISTANCE])
                .estimate_slot_and_duration_ms()
                .0,
            3 + MAX_SLOT_SKIP_DISTANCE,
        );

        assert_eq!(
            RecentLeaderSlots::from(vec![1, 100])
                .estimate_slot_and_duration_ms()
                .0,
            2
        );
        assert_eq!(
            RecentLeaderSlots::from(vec![1, 2, 100])
                .estimate_slot_and_duration_ms()
                .0,
            3
        );
        assert_eq!(
            RecentLeaderSlots::from(vec![1, 2, 3, 100])
                .estimate_slot_and_duration_ms()
                .0,
            4
        );
        assert_eq!(
            RecentLeaderSlots::from(vec![1, 2, 3, 99, 100])
                .estimate_slot_and_duration_ms()
                .0,
            4
        );
    }

    #[test]
    fn test_estimate_slot_window_start_fallback() {
        let start = |slot, timestamp| SlotEvent::Start { slot, timestamp };
        let end = |slot, timestamp| SlotEvent::End { slot, timestamp };
        let cases = [
            ("missing start", vec![end(7, 1750)], Some(3350)),
            (
                "start arrives after end",
                vec![end(7, 1750), start(8, 1800)],
                Some(3400),
            ),
            (
                "end arrives after start",
                vec![start(8, 1800), end(7, 1850)],
                Some(3400),
            ),
            (
                "duplicate start preserves first observation",
                vec![end(7, 1750), start(8, 1800), start(9, 2200), start(8, 2300)],
                Some(3400),
            ),
            ("both anchors missing", vec![start(9, 2200)], None),
        ];

        for (name, events, expected_end_ms) in cases {
            let mut recent_slots = RecentLeaderSlots::new();
            // Start-to-start timing gives 400 ms per slot, or 1600 ms per window.
            for event in [start(6, 1000), start(7, 1400)].into_iter().chain(events) {
                recent_slots.record(event);
            }

            // Use End(7) only without Start(8), regardless of arrival order.
            assert_eq!(
                recent_slots.estimate_slot().leader_window_end_ms,
                expected_end_ms,
                "{name}",
            );
        }
    }

    #[test]
    fn test_estimate_slot_duration_ms() {
        let start = |slot, timestamp| SlotEvent::Start { slot, timestamp };
        let end = |slot, timestamp| SlotEvent::End { slot, timestamp };
        let cases = [
            ("one start", vec![start(1, 1000)], None),
            ("only ends", vec![end(1, 1000), end(2, 1400)], None),
            (
                "one start and one end",
                vec![start(1, 1000), end(1, 1400)],
                None,
            ),
            ("duplicate slot", vec![start(1, 1000), start(1, 1400)], None),
            (
                "equal timestamps",
                vec![start(1, 1000), start(2, 1000)],
                None,
            ),
            (
                "backward timestamps",
                vec![start(1, 1400), start(2, 1000)],
                None,
            ),
            (
                "duration rounds to zero",
                vec![start(1, 1000), start(3, 1001)],
                None,
            ),
            (
                "consecutive slots",
                vec![start(1, 1000), start(2, 1400)],
                Some(400),
            ),
            (
                "missing notifications",
                vec![start(1, 1000), start(5, 2600)],
                Some(400),
            ),
            (
                "out of order with uneven intervals and end timestamps ignored",
                vec![
                    start(3, 1800),
                    end(3, 9999),
                    start(1, 1000),
                    end(1, 9999),
                    start(2, 1600),
                ],
                Some(400),
            ),
            (
                "future outlier",
                vec![
                    start(1, 1000),
                    start(2, 1400),
                    start(3, 1800),
                    start(100, 99999),
                ],
                Some(400),
            ),
            (
                "only one start survives filtering",
                vec![start(1, 1000), start(100, 99999)],
                None,
            ),
            (
                "end events contribute to the future-slot cutoff",
                vec![
                    start(1, 1000),
                    end(30, 10000),
                    end(31, 11000),
                    end(32, 12000),
                    end(33, 13000),
                    start(60, 24600),
                ],
                Some(400),
            ),
        ];

        for (name, events, expected) in cases {
            let mut recent_slots = RecentLeaderSlots::new();
            for event in events {
                recent_slots.record(event);
            }
            assert_eq!(
                recent_slots.estimate_slot_and_duration_ms().1,
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn test_estimate_slot_duration_ms_after_window_eviction() {
        let mut recent_slots = RecentLeaderSlots::new();
        recent_slots.record(SlotEvent::Start {
            slot: 0,
            timestamp: 0,
        });
        for slot in 1..=RECENT_LEADER_SLOTS_CAPACITY as u64 {
            recent_slots.record(SlotEvent::Start {
                slot,
                timestamp: 1000 + slot * 400,
            });
        }

        // The initial sample must no longer affect the duration of the retained slots.
        assert_eq!(recent_slots.estimate_slot_and_duration_ms().1, Some(400));
    }
}
