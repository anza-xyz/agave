//! Controls the queueing and firing of skip timer events for use
//! in the event loop.
// TODO: Make this mockable in event_handler for tests

mod stats;
mod timers;

use {
    crate::{event::VotorEvent, DELTA_BLOCK, DELTA_TIMEOUT},
    crossbeam_channel::Sender,
    parking_lot::RwLock,
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
    timers: Arc<RwLock<Timers>>,
    handle: JoinHandle<()>,
}

impl TimerManager {
    pub(crate) fn new(event_sender: Sender<VotorEvent>, exit: Arc<AtomicBool>) -> Self {
        let timers = Arc::new(RwLock::new(Timers::new(
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
}
