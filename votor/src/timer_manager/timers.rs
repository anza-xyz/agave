use {
    crate::{event::VotorEvent, timer_manager::stats::TimerManagerStats},
    crossbeam_channel::Sender,
    solana_clock::{NUM_CONSECUTIVE_LEADER_SLOTS, Slot},
    solana_runtime::leader_schedule_utils::last_of_consecutive_leader_slots,
    std::{
        cmp::Reverse,
        collections::{BinaryHeap, HashMap, VecDeque},
        time::{Duration, Instant},
    },
};

/// Dynamic skip timeouts can be increased to a maximum of 1hour.
const MAX_TIMEOUT_SECS: f64 = 3600.0;

/// Calculate the timeout multiplier based on standstill state.
/// Returns 1.0 if not in standstill, or 1.05^n where n is the number of
/// leader windows since standstill started.
fn calculate_timeout_multiplier(slot: Slot, standstill_slot: Option<Slot>) -> f64 {
    match standstill_slot {
        None => 1.0,
        Some(standstill_slot) => {
            // Calculate number of leader windows since standstill
            let slots_since_standstill = slot.saturating_sub(standstill_slot);
            let leader_windows = slots_since_standstill / NUM_CONSECUTIVE_LEADER_SLOTS;
            // Extend timeout by 5% for each leader window
            1.05_f64.powi(leader_windows as i32)
        }
    }
}

/// Encodes a basic state machine of the different stages involved in handling
/// timeouts for a window of slots.
enum TimerState {
    /// Waiting for the DELTA_TIMEOUT stage.
    WaitDeltaTimeout {
        /// The slots in the window.  Must not be empty.
        window: VecDeque<Slot>,
        /// Time when this stage will end.
        timeout: Instant,
        /// The scaled delta_block duration for this timer.
        scaled_delta_block: Duration,
    },
    /// Waiting for the DELTA_BLOCK stage.
    WaitDeltaBlock {
        /// The slots in the window.  Must not be empty.
        window: VecDeque<Slot>,
        /// Time when this stage will end.
        timeout: Instant,
        /// The scaled delta_block duration for this timer.
        scaled_delta_block: Duration,
    },
    /// The state machine is done.
    Done,
}

impl TimerState {
    /// Creates a new instance of the state machine.
    ///
    /// The `timeout_multiplier` is used to extend the timeout durations (e.g., 1.05 = 5% longer).
    /// Also returns the next time the timer should fire.
    fn new(
        slot: Slot,
        delta_timeout: Duration,
        delta_block: Duration,
        now: Instant,
        timeout_multiplier: f64,
    ) -> (Self, Instant) {
        let window = (slot..=last_of_consecutive_leader_slots(slot)).collect::<VecDeque<_>>();
        assert!(!window.is_empty());
        // Scale the timeouts by the multiplier, capping at 1 hour.
        let scaled_delta_timeout = Duration::from_secs_f64(
            (delta_timeout.as_secs_f64() * timeout_multiplier).min(MAX_TIMEOUT_SECS),
        );
        let scaled_delta_block = Duration::from_secs_f64(
            (delta_block.as_secs_f64() * timeout_multiplier).min(MAX_TIMEOUT_SECS),
        );
        let timeout = now.checked_add(scaled_delta_timeout).unwrap();
        (
            Self::WaitDeltaTimeout {
                window,
                timeout,
                scaled_delta_block,
            },
            timeout,
        )
    }

    /// Call to make progress on the state machine.
    ///
    /// Returns a potentially empty list of events that should be sent.
    fn progress(&mut self, now: Instant) -> Option<VotorEvent> {
        match self {
            Self::WaitDeltaTimeout {
                window,
                timeout,
                scaled_delta_block,
            } => {
                assert!(!window.is_empty());
                if &now < timeout {
                    return None;
                }
                let slot = *window.front().unwrap();
                let new_timeout = now.checked_add(*scaled_delta_block).unwrap();
                *self = Self::WaitDeltaBlock {
                    window: window.to_owned(),
                    timeout: new_timeout,
                    scaled_delta_block: *scaled_delta_block,
                };
                Some(VotorEvent::TimeoutCrashedLeader(slot))
            }
            Self::WaitDeltaBlock {
                window,
                timeout,
                scaled_delta_block,
            } => {
                assert!(!window.is_empty());
                if &now < timeout {
                    return None;
                }

                let ret = Some(VotorEvent::Timeout(window.pop_front().unwrap()));
                match window.front() {
                    None => *self = Self::Done,
                    Some(_next_slot) => {
                        *timeout = now.checked_add(*scaled_delta_block).unwrap();
                    }
                }
                ret
            }
            Self::Done => None,
        }
    }

    /// When would this state machine next be able to make progress.
    fn next_fire(&self) -> Option<Instant> {
        match self {
            Self::WaitDeltaTimeout { timeout, .. } | Self::WaitDeltaBlock { timeout, .. } => {
                Some(*timeout)
            }
            Self::Done => None,
        }
    }
}

/// Maintains all active timer states for windows of slots.
pub(super) struct Timers {
    delta_timeout: Duration,
    delta_block: Duration,
    /// Timers are indexed by slots.
    timers: HashMap<Slot, TimerState>,
    /// A min heap based on the time the next timer state might be ready.
    heap: BinaryHeap<Reverse<(Instant, Slot)>>,
    /// Channel to send events on.
    event_sender: Sender<VotorEvent>,
    /// Stats for the timer manager.
    stats: TimerManagerStats,
}

impl Timers {
    pub(super) fn new(
        delta_timeout: Duration,
        delta_block: Duration,
        event_sender: Sender<VotorEvent>,
    ) -> Self {
        Self {
            delta_timeout,
            delta_block,
            timers: HashMap::new(),
            heap: BinaryHeap::new(),
            event_sender,
            stats: TimerManagerStats::new(),
        }
    }

    /// Call to set timeouts for a new window of slots.
    /// If `standstill_slot` is provided, timeouts are extended by 5% for each leader window
    /// since standstill started.
    pub(super) fn set_timeouts(&mut self, slot: Slot, now: Instant, standstill_slot: Option<Slot>) {
        assert_eq!(self.heap.len(), self.timers.len());
        let timeout_multiplier = calculate_timeout_multiplier(slot, standstill_slot);
        let (timer, next_fire) = TimerState::new(
            slot,
            self.delta_timeout,
            self.delta_block,
            now,
            timeout_multiplier,
        );
        // It is possible that this slot already has a timer set e.g. if there
        // are multiple ParentReady for the same slot.  Do not insert new timer then.
        let mut new_timer_inserted = false;
        self.timers.entry(slot).or_insert_with(|| {
            self.heap.push(Reverse((next_fire, slot)));
            new_timer_inserted = true;
            timer
        });
        self.stats
            .incr_timeout_count_with_heap_size(self.heap.len(), new_timer_inserted);
    }

    /// Call to make progress on the timer states.  If there are still active
    /// timer states, returns when the earliest one might become ready.
    pub(super) fn progress(&mut self, now: Instant) -> Option<Instant> {
        assert_eq!(self.heap.len(), self.timers.len());
        let mut ret_timeout = None;
        loop {
            assert_eq!(self.heap.len(), self.timers.len());
            match self.heap.pop() {
                None => break,
                Some(Reverse((next_fire, slot))) => {
                    if now < next_fire {
                        ret_timeout =
                            Some(ret_timeout.map_or(next_fire, |r| std::cmp::min(r, next_fire)));
                        self.heap.push(Reverse((next_fire, slot)));
                        break;
                    }

                    let mut timer = self.timers.remove(&slot).unwrap();
                    if let Some(event) = timer.progress(now) {
                        self.event_sender.send(event).unwrap();
                    }
                    if let Some(next_fire) = timer.next_fire() {
                        self.heap.push(Reverse((next_fire, slot)));
                        assert!(self.timers.insert(slot, timer).is_none());
                        ret_timeout =
                            Some(ret_timeout.map_or(next_fire, |r| std::cmp::min(r, next_fire)));
                    }
                }
            }
        }
        ret_timeout
    }

    #[cfg(test)]
    pub(super) fn stats(&self) -> TimerManagerStats {
        self.stats.clone()
    }

    #[cfg(test)]
    pub(super) fn is_timeout_set(&self, slot: Slot) -> bool {
        self.timers.contains_key(&slot)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::common::{DELTA_BLOCK, DELTA_TIMEOUT},
        crossbeam_channel::unbounded,
    };

    #[test]
    fn timer_state_machine() {
        let one_micro = Duration::from_micros(1);
        let now = Instant::now();
        let slot = 0;
        let (mut timer_state, next_fire) = TimerState::new(slot, one_micro, one_micro, now, 1.0);

        assert!(matches!(
            timer_state.progress(next_fire).unwrap(),
            VotorEvent::TimeoutCrashedLeader(0)
        ));

        assert!(matches!(
            timer_state
                .progress(timer_state.next_fire().unwrap())
                .unwrap(),
            VotorEvent::Timeout(0)
        ));

        assert!(matches!(
            timer_state
                .progress(timer_state.next_fire().unwrap())
                .unwrap(),
            VotorEvent::Timeout(1)
        ));

        assert!(matches!(
            timer_state
                .progress(timer_state.next_fire().unwrap())
                .unwrap(),
            VotorEvent::Timeout(2)
        ));

        assert!(matches!(
            timer_state
                .progress(timer_state.next_fire().unwrap())
                .unwrap(),
            VotorEvent::Timeout(3)
        ));
        assert!(timer_state.next_fire().is_none());
    }

    #[test]
    fn timers_progress() {
        let one_micro = Duration::from_micros(1);
        let mut now = Instant::now();
        let (sender, receiver) = unbounded();
        let mut timers = Timers::new(one_micro, one_micro, sender);
        assert!(timers.progress(now).is_none());
        assert!(receiver.try_recv().unwrap_err().is_empty());

        timers.set_timeouts(0, now, None);
        while timers.progress(now).is_some() {
            now = now.checked_add(one_micro).unwrap();
        }
        let mut events = receiver.try_iter().collect::<Vec<_>>();

        assert!(matches!(
            events.remove(0),
            VotorEvent::TimeoutCrashedLeader(0)
        ));
        assert!(matches!(events.remove(0), VotorEvent::Timeout(0)));
        assert!(matches!(events.remove(0), VotorEvent::Timeout(1)));
        assert!(matches!(events.remove(0), VotorEvent::Timeout(2)));
        assert!(matches!(events.remove(0), VotorEvent::Timeout(3)));
        assert!(events.is_empty());
        let stats = timers.stats();
        assert_eq!(stats.set_timeout_count(), 1);
        assert_eq!(stats.set_timeout_succeed_count(), 1);
        assert_eq!(stats.max_heap_size(), 1);
    }

    #[test]
    fn timer_state_with_multiplier() {
        // Test that timeout multiplier correctly extends the timeout duration
        let delta_timeout = Duration::from_millis(100);
        let delta_block = Duration::from_millis(50);
        let now = Instant::now();
        let slot = 0;
        let multiplier = 1.5; // 50% longer timeouts

        let (mut timer_state, next_fire) =
            TimerState::new(slot, delta_timeout, delta_block, now, multiplier);

        // The first timeout should fire at now + (delta_timeout * 1.5) = now + 150ms
        let expected_first_fire = now + Duration::from_millis(150);
        assert!(
            next_fire >= expected_first_fire - Duration::from_micros(100)
                && next_fire <= expected_first_fire + Duration::from_micros(100),
            "Expected first fire around {expected_first_fire:?}, got {next_fire:?}",
        );

        // Progress the timer to get TimeoutCrashedLeader
        assert!(matches!(
            timer_state.progress(next_fire).unwrap(),
            VotorEvent::TimeoutCrashedLeader(0)
        ));

        // The next fire should be at next_fire + (delta_block * 1.5) = next_fire + 75ms
        let next = timer_state.next_fire().unwrap();
        let expected_delta = Duration::from_millis(75);
        let actual_delta = next - next_fire;
        assert!(
            actual_delta >= expected_delta - Duration::from_micros(100)
                && actual_delta <= expected_delta + Duration::from_micros(100),
            "Expected delta around {expected_delta:?}, got {actual_delta:?}",
        );
    }

    #[test]
    fn test_calculate_timeout_multiplier() {
        // No standstill - multiplier should be 1.0
        assert_eq!(calculate_timeout_multiplier(100, None), 1.0);

        // Standstill at slot 0
        // At slot 0 (same slot) - 0 leader windows passed
        assert_eq!(calculate_timeout_multiplier(0, Some(0)), 1.0);

        // At slot 4 (1 leader window = 4 slots) - 1.05^1
        let multiplier = calculate_timeout_multiplier(4, Some(0));
        assert!((multiplier - 1.05).abs() < 0.001);

        // At slot 8 (2 leader windows) - 1.05^2
        let multiplier = calculate_timeout_multiplier(8, Some(0));
        assert!((multiplier - 1.1025).abs() < 0.001);

        // At slot 40 (10 leader windows) - 1.05^10
        let multiplier = calculate_timeout_multiplier(40, Some(0));
        let expected = 1.05_f64.powi(10);
        assert!((multiplier - expected).abs() < 0.001);

        // Standstill at slot 20, current slot 28 (2 leader windows)
        let multiplier = calculate_timeout_multiplier(28, Some(20));
        assert!((multiplier - 1.1025).abs() < 0.001);
    }

    #[test]
    fn timer_state_caps_at_max_timeout() {
        let now = Instant::now();
        // Use a large multiplier that would exceed MAX_TIMEOUT
        let multiplier = 1000000.0;

        let (mut timer_state, next_fire) =
            TimerState::new(100, DELTA_TIMEOUT, DELTA_BLOCK, now, multiplier);

        // The first timeout should be capped at MAX_TIMEOUT
        let expected_first_fire = now + Duration::from_secs(MAX_TIMEOUT_SECS as u64);
        assert_eq!(next_fire, expected_first_fire);

        // Progress the timer to get TimeoutCrashedLeader
        assert!(matches!(
            timer_state.progress(next_fire).unwrap(),
            VotorEvent::TimeoutCrashedLeader(100)
        ));

        // The delta_block timeout should also be capped at MAX_TIMEOUT
        let next = timer_state.next_fire().unwrap();
        let actual_delta = next - next_fire;
        assert_eq!(actual_delta, Duration::from_secs(MAX_TIMEOUT_SECS as u64));
    }
}
