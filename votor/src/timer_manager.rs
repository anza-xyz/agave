//! Controls the queueing and firing of skip timer events for use
//! in the event loop.
// TODO: Make this mockable in event_handler for tests
mod stats;
mod timers;
use {
    crate::{
        common::{DELTA_BLOCK, DELTA_TIMEOUT},
        event::VotorEvent,
    },
    crossbeam_channel::Sender,
    parking_lot::RwLock as PlRwLock,
    solana_clock::Slot,
    std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    },
    timers::Timers,
};
/// A manager of timer states.  Uses a background thread to trigger next ready
/// timers and send events.
pub(crate) struct TimerManager {
    timers: Arc<PlRwLock<Timers>>,
    handle: JoinHandle<()>,
}

impl TimerManager {
    pub(crate) fn new(event_sender: Sender<VotorEvent>, exit: Arc<AtomicBool>) -> Self {
        let timers = Arc::new(PlRwLock::new(Timers::new(
            DELTA_TIMEOUT,
            DELTA_BLOCK,
            event_sender,
        )));
        let handle = {
            let timers = Arc::clone(&timers);
            thread::spawn(move || {
                while !exit.load(Ordering::Relaxed) {
                    let duration = match timers.write().progress(Instant::now()) {
                        None => {
                            // No active timers, sleep for an arbitrary amount.
                            // This should be smaller than the minimum amount
                            // of time any newly added timers would take to expire.
                            Duration::from_millis(100)
                        }
                        Some(next_fire) => next_fire.duration_since(Instant::now()),
                    };
                    thread::sleep(duration);
                }
            })
        };
        Self { timers, handle }
    }
    pub(crate) fn set_timeouts(&self, slot: Slot) {
        self.timers.write().set_timeouts(slot, Instant::now());
    }
    pub(crate) fn join(self) {
        self.handle.join().unwrap();
    }

    #[cfg(test)]
    pub(crate) fn is_timeout_set(&self, slot: Slot) -> bool {
        self.timers.read().is_timeout_set(slot)
    }
}

#[cfg(test)]
mod tests {
    use {super::*, crate::event::VotorEvent, crossbeam_channel::unbounded, std::time::Duration};
    #[test]
    fn test_timer_manager() {
        let (event_sender, event_receiver) = unbounded();
        let exit = Arc::new(AtomicBool::new(false));
        let timer_manager = TimerManager::new(event_sender, exit.clone());
        let slot = 52;
        let start = Instant::now();
        timer_manager.set_timeouts(slot);
        // Should see two timeouts at DELTA_BLOCK and DELTA_TIMEOUT
        let mut timeouts_received = 0;
        while timeouts_received < 2 && Instant::now().duration_since(start) < Duration::from_secs(2)
        {
            let res = event_receiver.recv_timeout(Duration::from_millis(200));
            if let Ok(event) = res {
                match event {
                    VotorEvent::Timeout(s) => {
                        assert_eq!(s, slot);
                        assert!(
                            Instant::now().duration_since(start) >= DELTA_TIMEOUT + DELTA_BLOCK
                        );
                        timeouts_received += 1;
                    }
                    VotorEvent::TimeoutCrashedLeader(s) => {
                        assert_eq!(s, slot);
                        assert!(Instant::now().duration_since(start) >= DELTA_TIMEOUT);
                        timeouts_received += 1;
                    }
                    _ => panic!("Unexpected event: {event:?}"),
                }
            }
        }
        assert!(
            timeouts_received == 2,
            "Did not receive all expected timeouts"
        );
        exit.store(true, Ordering::Relaxed);
        timer_manager.join();
    }
}
