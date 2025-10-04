use {
    crate::route::AtomicRouter,
    std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread::{self, JoinHandle},
        time::Duration,
    },
};

pub struct RouteMonitor;

impl RouteMonitor {
    pub fn start(
        atomic_router: Arc<AtomicRouter>,
        exit: Arc<AtomicBool>,
        update_interval: Duration,
    ) -> JoinHandle<()> {
        thread::spawn(move || {
            log::info!(
                "Starting route monitor with {}s interval",
                update_interval.as_secs()
            );

            while !exit.load(Ordering::Relaxed) {
                thread::park_timeout(update_interval);

                if exit.load(Ordering::Relaxed) {
                    break;
                }

                // Fetch and update both routes and ARP table atomically
                if let Err(e) = atomic_router.update_routes_and_neighbors() {
                    log::warn!("Failed to update routes and ARP table: {e}");
                }
            }
        })
    }
}
