use {
    crate::{
        netlink::{netlink_get_neighbors, netlink_get_routes},
        route::AtomicRouter,
    },
    libc::AF_INET,
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

                // Fetch routes
                match netlink_get_routes(AF_INET as u8) {
                    Ok(routes) => {
                        log::debug!("Fetched {} total routes from kernel", routes.len());
                        atomic_router.update_routes(routes);
                    }
                    Err(e) => {
                        log::warn!("Failed to fetch routes: {e}");
                    }
                }

                // Fetch ARP table
                match netlink_get_neighbors(None, AF_INET as u8) {
                    Ok(neighbors) => {
                        atomic_router.update_arp(neighbors);
                    }
                    Err(e) => {
                        log::warn!("Failed to fetch ARP table: {e}");
                    }
                }
            }
        })
    }
}
