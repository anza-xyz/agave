use {
    crate::route::AtomicRouter,
    std::{
        sync::Arc,
        thread::{self, JoinHandle},
        time::Duration,
    },
};

pub struct RouteMonitor;

impl RouteMonitor {
    pub fn start(atomic_router: Arc<AtomicRouter>, update_interval: Duration) -> JoinHandle<()> {
        thread::spawn(move || {
            log::info!(
                "Starting route monitor with {}s interval",
                update_interval.as_secs()
            );

            loop {
                thread::sleep(update_interval);

                // Fetch and update both routes and ARP table atomically
                match atomic_router.update_routes_and_neighbors() {
                    Ok(()) => {
                        log::info!("greg: Successfully updated routes and ARP table");
                    }
                    Err(e) => {
                        log::info!("Failed to update routes and ARP table: {e}");
                    }
                }
            }
        })
    }
}
