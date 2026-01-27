use {
    crate::{
        netlink::{parse_rtm_newneigh, parse_rtm_newroute, NetlinkMessage, NetlinkSocket},
        route::Router,
    },
    arc_swap::ArcSwap,
    libc::{
        self, pollfd, POLLERR, POLLHUP, POLLIN, POLLNVAL, RTMGRP_IPV4_ROUTE, RTMGRP_NEIGH,
        RTM_DELNEIGH, RTM_DELROUTE, RTM_NEWNEIGH, RTM_NEWROUTE,
    },
    log::*,
    std::{
        io::{Error, ErrorKind},
        net::IpAddr,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread,
        time::{Duration, Instant},
    },
};
pub struct RouteMonitor;

impl RouteMonitor {
    /// Subscribes to RTMGRP_IPV4_ROUTE | RTMGRP_NEIGH multicast groups
    /// Waits for updates to arrive on the netlink socket
    /// Publishes the updated routing table every `update_interval` if needed
    pub fn start(
        atomic_router: Arc<ArcSwap<Router>>,
        exit: Arc<AtomicBool>,
        update_interval: Duration,
    ) -> thread::JoinHandle<()> {
        thread::Builder::new()
            .name("solRouteMon".to_string())
            .spawn(move || {
                let mut state =
                    RouteMonitorState::new(Router::new().expect("error creating Router"));

                let timeout = Duration::from_millis(10);
                while !exit.load(Ordering::Relaxed) {
                    state.publish_if_needed(&atomic_router, update_interval);

                    let mut pfd = pollfd {
                        fd: state.sock.as_raw_fd(),
                        events: POLLIN,
                        revents: 0,
                    };

                    let ev = match poll(&mut pfd, timeout) {
                        // timeout
                        Ok(0) => continue,
                        Ok(_) => pfd.revents,
                        Err(e) => {
                            error!("netlink poll error: {e}");
                            state.reset(&atomic_router);
                            continue;
                        }
                    };

                    debug_assert!(ev & POLLNVAL == 0);

                    if (ev & (POLLHUP | POLLERR)) != 0 {
                        error!(
                            "netlink poll error (revents={}{})",
                            if ev & POLLERR != 0 { "POLLERR " } else { "" },
                            if ev & POLLHUP != 0 { "POLLHUP" } else { "" },
                        );
                        state.reset(&atomic_router);
                        continue;
                    }
                    if (ev & POLLIN) == 0 {
                        continue;
                    }
                    // Drain channel
                    match state.sock.recv() {
                        Ok(msgs) => {
                            state.dirty |= Self::process_netlink_updates(&mut state.router, &msgs);
                        }
                        Err(e) => {
                            error!("netlink recv error: {e}");
                            state.reset(&atomic_router);
                            continue;
                        }
                    }
                }
            })
            .unwrap()
    }

    #[inline]
    fn process_netlink_updates(router: &mut Router, msgs: &[NetlinkMessage]) -> bool {
        let mut dirty = false;
        for m in msgs {
            match m.header.nlmsg_type {
                RTM_NEWROUTE => {
                    if let Some(r) = parse_rtm_newroute(m) {
                        dirty |= router.upsert_route(r);
                    }
                }
                RTM_DELROUTE => {
                    if let Some(r) = parse_rtm_newroute(m) {
                        dirty |= router.remove_route(r);
                    }
                }
                RTM_NEWNEIGH => {
                    if let Some(n) = parse_rtm_newneigh(m, None) {
                        if let Some(IpAddr::V4(_)) = n.destination {
                            dirty |= router.upsert_neighbor(n);
                        }
                    }
                }
                RTM_DELNEIGH => {
                    if let Some(n) = parse_rtm_newneigh(m, None) {
                        if let Some(IpAddr::V4(ip)) = n.destination {
                            dirty |= router.remove_neighbor(ip, n.ifindex as u32);
                        }
                    }
                }
                _ => {}
            }
        }
        dirty
    }
}

struct RouteMonitorState {
    sock: NetlinkSocket,
    router: Router,
    dirty: bool,
    last_publish: Instant,
}

impl RouteMonitorState {
    /// Creates a new RouteMonitorState with a bounded netlink socket
    fn new(router: Router) -> Self {
        Self {
            sock: NetlinkSocket::bind((RTMGRP_IPV4_ROUTE | RTMGRP_NEIGH) as u32)
                .expect("error creating netlink socket"),
            router,
            dirty: false,
            last_publish: Instant::now(),
        }
    }

    /// Resets the route monitor state by creating a new router and reinitializing
    /// the netlink socket. Used when errors occur to recover to a clean state
    fn reset(&mut self, atomic_router: &Arc<ArcSwap<Router>>) {
        atomic_router.store(Arc::new(Router::new().expect("error creating Router")));
        *self = Self::new(Arc::unwrap_or_clone(atomic_router.load_full()));
    }

    /// Publishes the updated router if there are new route/neighbor updates
    /// and the update interval has elapsed
    fn publish_if_needed(
        &mut self,
        atomic_router: &Arc<ArcSwap<Router>>,
        update_interval: Duration,
    ) {
        if self.dirty && self.last_publish.elapsed() >= update_interval {
            atomic_router.store(Arc::new(self.router.clone()));
            self.last_publish = Instant::now();
            self.dirty = false;
        }
    }
}

/// Wrapper around libc::poll. Polls the netlink socket for incoming events
#[inline]
fn poll(pfd: &mut pollfd, timeout: Duration) -> Result<i32, Error> {
    let rc = loop {
        // Safety: pfd can't be NULL as references can't be NULL
        let rc = unsafe { libc::poll(pfd as *mut pollfd, 1, timeout.as_millis() as i32) };
        if rc < 0 && Error::last_os_error().kind() == ErrorKind::Interrupted {
            continue;
        }
        break rc;
    };
    if rc < 0 {
        return Err(Error::last_os_error());
    }
    Ok(rc)
}
