use {
    crate::{event::VotorEvent, timer_manager::stats::TimerManagerStats},
    crossbeam_channel::Sender,
    solana_clock::Slot,
    solana_leader_schedule::NUM_CONSECUTIVE_LEADER_SLOTS,
    solana_runtime::leader_schedule_utils::last_of_consecutive_leader_slots,
    std::{
        cmp::Reverse,
        collections::{BinaryHeap, HashMap, VecDeque},
        time::{Duration, Instant},
    },
};

/// Dynamic skip timeouts can be increased to a maximum of 1hour.
const MAX_TIMEOUT: Duration = Duration::from_secs(3600);

/// Calculate the timeout multiplier based on standstill state.
/// Returns 1.0 if not in standstill, or 1.05^n where n is the number of
/// leader windows since standstill started.
fn calculate_timeout_multiplier(slot: Slot, standstill_slot: Option<Slot>) -> f64 {
    match standstill_slot {
        None => 1.0,
        Some(standstill_slot) => {
            // Calculate number of leader windows since standstill
            let slots_since_standstill = slot.saturating_sub(standstill_slot);
            let leader_windows = slots_since_standstill
                .checked_div(NUM_CONSECUTIVE_LEADER_SLOTS.get() as Slot)
                .expect("NUM_CONSECUTIVE_LEADER_SLOTS is non-zero");
            // Extend timeout by 5% for each leader window
            1.05_f64.powi(leader_windows as i32)
        }
    }
}

#[derive(Clone)]
struct TimeoutConfig {
    delta_timeout: Duration,
    delta_first_slice: Duration,
    delta_block: Duration,
}

impl TimeoutConfig {
    fn scaled(&self, slot: Slot, standstill_slot: Option<Slot>) -> TimeoutConfig {
        let multiplier = calculate_timeout_multiplier(slot, standstill_slot);
        let delta_timeout = Duration::try_from_secs_f64(
            (self.delta_timeout.as_secs_f64() * multiplier).clamp(0.0, MAX_TIMEOUT.as_secs_f64()),
        )
        .unwrap_or(MAX_TIMEOUT);
        TimeoutConfig {
            delta_timeout,
            delta_first_slice: self.delta_first_slice,
            delta_block: self.delta_block,
        }
    }
}

/// Encodes a basic state machine of the different stages involved in handling
/// timeouts for a window of slots.
enum TimerState {
    /// Waiting for initial DELTA_TIMEOUT + delta_first_slice stage.
    WaitForFirstSlice {
        /// The slots in the window.  Must not be empty.
        window: VecDeque<Slot>,
        /// Time when this stage will end.
        timeout: Instant,
        /// Timeout constituent parameters.
        timeout_config: TimeoutConfig,
    },
    /// Waiting for DELTA_TIMEOUT + i * DELTA_BLOCK for each block i in window.
    WaitForBlock {
        /// The slots in the window.  Must not be empty.
        window: VecDeque<Slot>,
        /// Time when this stage will end.
        timeout: Instant,
        /// Timeout constituent parameters.
        timeout_config: TimeoutConfig,
    },
    /// The state machine is done.
    Done,
}

impl TimerState {
    /// Creates a new instance of the state machine.
    ///
    /// The `timeout_multiplier` extends only the network-DELTA-derived timeout
    /// (`delta_timeout`). `delta_first_slice` and `delta_block` are protocol
    /// pacing, not a function of network delays, so they are not scaled. Also
    /// returns the next time the timer should fire.
    fn new(slot: Slot, timeout_config: TimeoutConfig, now: Instant) -> (Self, Instant) {
        let window = (slot..=last_of_consecutive_leader_slots(slot)).collect::<VecDeque<_>>();
        assert!(!window.is_empty());

        // A correct leader may take up to `delta_first_slice` to send their first slice,
        // so the earliest sound point to declare them crashed is
        // `delta_first_slice + delta_timeout` after their window starts.
        let timeout = now
            .checked_add(timeout_config.delta_timeout)
            .unwrap()
            .checked_add(timeout_config.delta_first_slice)
            .unwrap();
        (
            Self::WaitForFirstSlice {
                window,
                timeout,
                timeout_config: timeout_config.clone(),
            },
            timeout,
        )
    }

    /// Call to make progress on the state machine.
    ///
    /// Returns a potentially empty list of events that should be sent.
    fn progress(&mut self, now: Instant) -> Option<VotorEvent> {
        match self {
            Self::WaitForFirstSlice {
                window,
                timeout,
                timeout_config,
            } => {
                assert!(!window.is_empty());
                if &now < timeout {
                    return None;
                }
                let slot = *window.front().unwrap();
                // Slot 0's block deadline is `T + delta_block + scaled_delta_timeout`;
                // subtract the `delta_first_slice` paid up-front to `WaitForFirstSlice`.
                let new_timeout = timeout
                    .checked_add(timeout_config.delta_block)
                    .unwrap()
                    .checked_sub(timeout_config.delta_first_slice)
                    .unwrap();
                *self = Self::WaitForBlock {
                    window: window.to_owned(),
                    timeout: new_timeout,
                    timeout_config: timeout_config.clone(),
                };
                Some(VotorEvent::TimeoutCrashedLeader(slot))
            }
            Self::WaitForBlock {
                window,
                timeout,
                timeout_config,
            } => {
                assert!(!window.is_empty());
                if &now < timeout {
                    return None;
                }

                let ret = Some(VotorEvent::Timeout(window.pop_front().unwrap()));
                match window.front() {
                    None => *self = Self::Done,
                    Some(_next_slot) => {
                        *timeout = timeout.checked_add(timeout_config.delta_block).unwrap();
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
            Self::WaitForFirstSlice { timeout, .. } | Self::WaitForBlock { timeout, .. } => {
                Some(*timeout)
            }
            Self::Done => None,
        }
    }
}

/// Maintains all active timer states for windows of slots.
pub(super) struct Timers {
    delta_timeout: Duration,
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
    pub(super) fn new(delta_timeout: Duration, event_sender: Sender<VotorEvent>) -> Self {
        Self {
            delta_timeout,
            timers: HashMap::new(),
            heap: BinaryHeap::new(),
            event_sender,
            stats: TimerManagerStats::new(),
        }
    }

    /// Call to set timeouts for a new window of slots.
    /// If `standstill_slot` is provided, timeouts are extended by 5% for each leader window
    /// since standstill started.
    pub(super) fn set_timeouts(
        &mut self,
        slot: Slot,
        now: Instant,
        standstill_slot: Option<Slot>,
        delta_first_slice: Duration,
        delta_block: Duration,
    ) -> bool {
        assert!(delta_first_slice <= delta_block);
        assert_eq!(self.heap.len(), self.timers.len());
        let timeout_config = TimeoutConfig {
            delta_timeout: self.delta_timeout,
            delta_first_slice,
            delta_block,
        }
        .scaled(slot, standstill_slot);
        let (timer, next_fire) = TimerState::new(slot, timeout_config, now);
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
        new_timer_inserted
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
        super::*, crate::common::DELTA_TIMEOUT, crossbeam_channel::unbounded,
        solana_clock::DEFAULT_MS_PER_SLOT,
    };

    #[test]
    fn timer_state_machine() {
        let one_micro = Duration::from_micros(1);
        let now = Instant::now();
        let slot = 0;
        let timeout_config = TimeoutConfig {
            delta_timeout: one_micro,
            delta_first_slice: one_micro,
            delta_block: one_micro,
        }
        .scaled(slot, None);
        let (mut timer_state, next_fire) = TimerState::new(slot, timeout_config, now);

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
        let mut timers = Timers::new(one_micro, sender);
        assert!(timers.progress(now).is_none());
        assert!(receiver.try_recv().unwrap_err().is_empty());
        let delta_block = Duration::from_millis(DEFAULT_MS_PER_SLOT);

        timers.set_timeouts(0, now, None, delta_block, delta_block);
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
        let delta_first_slice = Duration::from_millis(10);
        let delta_block = Duration::from_millis(50);
        let now = Instant::now();
        let slot = 0;

        let timeout_config = TimeoutConfig {
            delta_timeout,
            delta_first_slice,
            delta_block,
        }
        // 1 leader window standstill -> 1.05x multiplier
        .scaled(NUM_CONSECUTIVE_LEADER_SLOTS.get() as Slot, Some(0));
        let (mut timer_state, next_fire) = TimerState::new(slot, timeout_config, now);

        // The first timeout should fire at now + (delta_timeout * 1.05) + delta_first_slice
        // = now + 105ms + delta_first_slice
        let expected_first_fire = now + Duration::from_millis(105) + delta_first_slice;
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

        // Slot 0's block deadline is `now + delta_block + scaled_delta_timeout`
        // = now + 50ms + 105ms = now + 155ms (unaffected by delta_first_slice).
        let next = timer_state.next_fire().unwrap();
        let expected_next = now + Duration::from_millis(155);
        let skew = if next >= expected_next {
            next - expected_next
        } else {
            expected_next - next
        };
        assert!(
            skew <= Duration::from_micros(100),
            "Expected slot-0 deadline around {expected_next:?}, got {next:?}",
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
        let delta_block = Duration::from_millis(DEFAULT_MS_PER_SLOT);
        let delta_first_slice = delta_block;

        let timeout_config = TimeoutConfig {
            delta_timeout: DELTA_TIMEOUT,
            delta_first_slice,
            delta_block,
        }
        // ensure a large multiplier that would exceed MAX_TIMEOUT
        .scaled(1000, Some(0));
        let (mut timer_state, next_fire) = TimerState::new(1000, timeout_config, now);

        // The first timeout should be capped at MAX_TIMEOUT (+ unscaled delta_first_slice).
        let expected_first_fire = now + MAX_TIMEOUT + delta_first_slice;
        assert_eq!(next_fire, expected_first_fire);

        // Progress the timer to get TimeoutCrashedLeader
        assert!(matches!(
            timer_state.progress(next_fire).unwrap(),
            VotorEvent::TimeoutCrashedLeader(1000)
        ));

        // Slot 0's block deadline is `T + delta_block + scaled_delta_timeout`, so
        // the gap from the (delta_first_slice-shifted) first fire is `delta_block - delta_first_slice`.
        let next = timer_state.next_fire().unwrap();
        let actual_delta = next - next_fire;
        assert_eq!(actual_delta, delta_block - delta_first_slice);
    }
}
