use {
    crate::{
        netlink::{netlink_get_neighbors, netlink_get_routes},
        route::{filter_routes, RouteUpdate},
    },
    libc::AF_INET,
    std::{
        thread::{self, JoinHandle},
        time::Duration,
    },
    tokio::sync::broadcast,
};

pub struct RouteMonitor {
    update_sender: broadcast::Sender<RouteUpdate>,
    update_interval: Duration,
}

impl RouteMonitor {
    pub fn new(update_sender: broadcast::Sender<RouteUpdate>, update_interval: Duration) -> Self {
        Self {
            update_sender,
            update_interval,
        }
    }

    pub fn start(&self) -> JoinHandle<()> {
        let update_sender = self.update_sender.clone();
        let update_interval = self.update_interval;

        thread::spawn(move || {
            log::info!(
                "greg: Starting route monitor with {}s interval",
                update_interval.as_secs()
            );

            loop {
                thread::sleep(update_interval);

                // Fetch routes
                match netlink_get_routes(AF_INET as u8) {
                    Ok(mut routes) => {
                        log::info!("greg: Fetched {} total routes from kernel", routes.len());
                        filter_routes(&mut routes);
                        log::info!("greg: Filtered routes down to {}", routes.len());
                        if update_sender
                            .send(RouteUpdate::RoutesChanged(routes))
                            .is_err()
                        {
                            log::warn!("greg: Route update channel full, dropping update");
                        }
                    }
                    Err(e) => {
                        log::warn!("greg:Failed to fetch routes: {e}");
                    }
                }

                // Fetch ARP table
                match netlink_get_neighbors(None, AF_INET as u8) {
                    Ok(neighbors) => {
                        if update_sender
                            .send(RouteUpdate::ArpChanged(neighbors))
                            .is_err()
                        {
                            log::warn!("greg: ARP update channel full, dropping update");
                        }
                    }
                    Err(e) => {
                        log::warn!("greg: Failed to fetch ARP table: {e}");
                    }
                }
            }
        })
    }
}
