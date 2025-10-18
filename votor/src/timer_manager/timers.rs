use {
    crate::{
        consensus_metrics::ConsensusMetrics, event::VotorEvent,
        timer_manager::stats::TimerManagerStats,
    },
    crossbeam_channel::Sender,
    parking_lot::RwLock as PlRwLock,
    solana_clock::Slot,
    solana_ledger::leader_schedule_utils::last_of_consecutive_leader_slots,
    std::{
        cmp::Reverse,
        collections::{BinaryHeap, HashMap, VecDeque},
        sync::Arc,
        time::{Duration, Instant},
    },
};
/// Encodes a basic state machine of the different stages involved in handling
/// timeouts for a window of slots.
enum TimerState {
    /// Waiting for the DELTA_TIMEOUT stage.
    WaitDeltaTimeout {
        /// The slots in the window.  Must not be empty.
        window: VecDeque<Slot>,
        /// Time when this stage will end.
        timeout: Instant,
    },
    /// Waiting for the DELTA_BLOCK stage.
    WaitDeltaBlock {
        /// The slots in the window.  Must not be empty.
        window: VecDeque<Slot>,
        /// Time when this stage will end.
        timeout: Instant,
    },
    /// The state machine is done.
    Done,
}
impl TimerState {
    /// Creates a new instance of the state machine.
    ///
    /// Also returns the next time the timer should fire.
    fn new(slot: Slot, delta_timeout: Duration, now: Instant) -> (Self, Instant) {
        let window = (slot..=last_of_consecutive_leader_slots(slot)).collect::<VecDeque<_>>();
        assert!(!window.is_empty());
        let timeout = now.checked_add(delta_timeout).unwrap();
        (Self::WaitDeltaTimeout { window, timeout }, timeout)
    }
    /// Call to make progress on the state machine.
    ///
    /// Returns a potentially empty list of events that should be sent.
    fn progress(
        &mut self,
        delta_block: Duration,
        now: Instant,
        consensus_metrics: &PlRwLock<ConsensusMetrics>,
    ) -> Option<VotorEvent> {
        match self {
            Self::WaitDeltaTimeout { window, timeout } => {
                assert!(!window.is_empty());
                if &now < timeout {
                    return None;
                }
                let slot = *window.front().unwrap();
                consensus_metrics.write().record_start_of_slot(slot);
                let timeout = now.checked_add(delta_block).unwrap();
                *self = Self::WaitDeltaBlock {
                    window: window.to_owned(),
                    timeout,
                };
                Some(VotorEvent::TimeoutCrashedLeader(slot))
            }
            Self::WaitDeltaBlock { window, timeout } => {
                assert!(!window.is_empty());
                if &now < timeout {
                    return None;
                }

                let ret = Some(VotorEvent::Timeout(window.pop_front().unwrap()));
                match window.front() {
                    None => *self = Self::Done,
                    Some(next_slot) => {
                        consensus_metrics.write().record_start_of_slot(*next_slot);
                        *timeout = now.checked_add(delta_block).unwrap();
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
            Self::WaitDeltaTimeout { window: _, timeout }
            | Self::WaitDeltaBlock { window: _, timeout } => Some(*timeout),
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
    consensus_metrics: Arc<PlRwLock<ConsensusMetrics>>,
    /// Stats for the timer manager.
    stats: TimerManagerStats,
}
impl Timers {
    pub(super) fn new(
        delta_timeout: Duration,
        delta_block: Duration,
        event_sender: Sender<VotorEvent>,
        consensus_metrics: Arc<PlRwLock<ConsensusMetrics>>,
    ) -> Self {
        Self {
            delta_timeout,
            delta_block,
            timers: HashMap::new(),
            heap: BinaryHeap::new(),
            event_sender,
            consensus_metrics,
            stats: TimerManagerStats::new(),
        }
    }
    /// Call to set timeouts for a new window of slots.
    pub(super) fn set_timeouts(&mut self, slot: Slot, now: Instant) {
        assert_eq!(self.heap.len(), self.timers.len());
        let (timer, next_fire) = TimerState::new(slot, self.delta_timeout, now);
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
                    if let Some(event) =
                        timer.progress(self.delta_block, now, &self.consensus_metrics)
                    {
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
    use {super::*, crossbeam_channel::unbounded, solana_pubkey::Pubkey};

    #[test]
    fn timer_state_machine() {
        let leader = Pubkey::default();
        let consensus_metrics = PlRwLock::new(ConsensusMetrics::new(1));
        let one_micro = Duration::from_micros(1);
        let now = Instant::now();
        let slot = 0;
        let (mut timer_state, next_fire) = TimerState::new(slot, one_micro, now);

        assert!(matches!(
            timer_state
                .progress(one_micro, next_fire, &consensus_metrics)
                .unwrap(),
            VotorEvent::TimeoutCrashedLeader(0)
        ));
        consensus_metrics
            .write()
            .record_block_hash_seen(leader, 0)
            .unwrap();

        assert!(matches!(
            timer_state
                .progress(
                    one_micro,
                    timer_state.next_fire().unwrap(),
                    &consensus_metrics
                )
                .unwrap(),
            VotorEvent::Timeout(0)
        ));
        consensus_metrics
            .write()
            .record_block_hash_seen(leader, 1)
            .unwrap();

        assert!(matches!(
            timer_state
                .progress(
                    one_micro,
                    timer_state.next_fire().unwrap(),
                    &consensus_metrics
                )
                .unwrap(),
            VotorEvent::Timeout(1)
        ));
        consensus_metrics
            .write()
            .record_block_hash_seen(leader, 2)
            .unwrap();

        assert!(matches!(
            timer_state
                .progress(
                    one_micro,
                    timer_state.next_fire().unwrap(),
                    &consensus_metrics
                )
                .unwrap(),
            VotorEvent::Timeout(2)
        ));
        consensus_metrics
            .write()
            .record_block_hash_seen(leader, 3)
            .unwrap();

        assert!(matches!(
            timer_state
                .progress(
                    one_micro,
                    timer_state.next_fire().unwrap(),
                    &consensus_metrics
                )
                .unwrap(),
            VotorEvent::Timeout(3)
        ));
        assert!(timer_state.next_fire().is_none());
    }

    #[test]
    fn timers_progress() {
        let consensus_metrics = Arc::new(PlRwLock::new(ConsensusMetrics::new(1)));
        let one_micro = Duration::from_micros(1);
        let mut now = Instant::now();
        let (sender, receiver) = unbounded();
        let mut timers = Timers::new(one_micro, one_micro, sender, consensus_metrics);
        assert!(timers.progress(now).is_none());
        assert!(receiver.try_recv().unwrap_err().is_empty());

        timers.set_timeouts(0, now);
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
}
