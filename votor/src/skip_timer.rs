//! Controls the queueing and firing of skip timer events for use
//! in the event loop.
//! TODO: Make this mockable in event_handler for tests
use {
    crate::{
        event::{VotorEvent, VotorEventSender},
        skip_timeout, DELTA_BLOCK,
    },
    solana_clock::Slot,
    solana_ledger::leader_schedule_utils::{
        last_of_consecutive_leader_slots, remaining_slots_in_window,
    },
    std::{
        collections::{BinaryHeap, VecDeque},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, RwLock,
        },
        thread::{Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

#[derive(Debug, Copy, Clone)]
struct SkipTimer {
    id: u64,
    next_fire: Instant,
    interval: Duration,
    remaining: u64,
    start_slot: Slot,
}

impl SkipTimer {
    fn slot_to_fire(&self) -> Slot {
        let end_slot = last_of_consecutive_leader_slots(self.start_slot);
        end_slot
            .checked_sub(self.remaining)
            .unwrap()
            .checked_add(1)
            .unwrap()
    }
}

impl PartialEq for SkipTimer {
    fn eq(&self, other: &Self) -> bool {
        self.next_fire.eq(&other.next_fire)
    }
}
impl Eq for SkipTimer {}
impl PartialOrd for SkipTimer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(other.next_fire.cmp(&self.next_fire))
    }
}
impl Ord for SkipTimer {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.next_fire.cmp(&self.next_fire)
    }
}

pub(crate) struct SkipTimerManager {
    heap: BinaryHeap<SkipTimer>,
    order: VecDeque<u64>,
    max_timers: usize,
    next_id: u64,
}

impl SkipTimerManager {
    pub(crate) fn new(max_timers: usize) -> Self {
        SkipTimerManager {
            heap: BinaryHeap::new(),
            order: VecDeque::new(),
            max_timers,
            next_id: 0,
        }
    }

    pub(crate) fn set_timeouts(&mut self, start_slot: Slot) {
        // Evict oldest if needed
        if self.order.len() == self.max_timers {
            if let Some(evict_id) = self.order.pop_front() {
                self.heap = self.heap.drain().filter(|t| t.id != evict_id).collect();
            }
        }

        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);

        // To account for restarting from a snapshot in the middle of a leader window
        // or from genesis, we compute the exact length of this leader window:
        let remaining = remaining_slots_in_window(start_slot);
        // TODO: should we change the first fire as well?
        let next_fire = Instant::now().checked_add(skip_timeout(0)).unwrap();

        let timer = SkipTimer {
            id,
            next_fire,
            interval: DELTA_BLOCK,
            remaining,
            start_slot,
        };
        self.order.push_back(id);
        self.heap.push(timer);
    }
}

pub(crate) struct SkipTimerService {
    t_skip_timer: JoinHandle<()>,
}

impl SkipTimerService {
    pub(crate) fn new(
        exit: Arc<AtomicBool>,
        max_timers: usize,
        event_sender: VotorEventSender,
    ) -> (Self, Arc<RwLock<SkipTimerManager>>) {
        let manager = Arc::new(RwLock::new(SkipTimerManager::new(max_timers)));
        let manager_c = manager.clone();
        let t_skip_timer = Builder::new()
            .name("solSkipTimer".to_string())
            .spawn(move || Self::timer_thread(exit, manager_c, event_sender))
            .unwrap();
        (Self { t_skip_timer }, manager)
    }

    pub(crate) fn timer_thread(
        exit: Arc<AtomicBool>,
        manager: Arc<RwLock<SkipTimerManager>>,
        event_sender: VotorEventSender,
    ) {
        while !exit.load(Ordering::Relaxed) {
            let now = Instant::now();

            // Fire all timers that are due
            let mut manager_w = manager.write().unwrap();
            while let Some(mut timer) = manager_w.heap.peek().copied() {
                if timer.next_fire <= now {
                    manager_w.heap.pop();

                    // Send timeout event
                    // TODO: handle error
                    let slot = timer.slot_to_fire();
                    event_sender.send(VotorEvent::Timeout(slot)).unwrap();

                    timer.remaining = timer.remaining.checked_sub(1).unwrap();
                    if timer.remaining > 0 {
                        timer.next_fire = timer.next_fire.checked_add(timer.interval).unwrap();
                        manager_w.heap.push(timer);
                    } else {
                        // Remove from order list
                        manager_w.order.retain(|&x| x != timer.id);
                    }
                } else {
                    break;
                }
            }

            // Sleep until next timer or 100ms (small enough that a new timer added cannot fire)
            let sleep_duration = manager_w
                .heap
                .peek()
                .map(|t| t.next_fire.saturating_duration_since(Instant::now()))
                .unwrap_or(Duration::from_millis(100));
            drop(manager_w);
            std::thread::sleep(sleep_duration);
        }
    }
    pub(crate) fn join(self) -> std::thread::Result<()> {
        self.t_skip_timer.join()
    }
}
