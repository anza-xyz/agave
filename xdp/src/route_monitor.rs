use {
    crate::{
        netlink::{
            parse_rtm_ifinfomsg, parse_rtm_newneigh, parse_rtm_newroute, NetlinkMessage,
            NetlinkSocket,
        },
        route::Router,
    },
    arc_swap::ArcSwap,
    libc::{
        self, pollfd, POLLERR, POLLHUP, POLLIN, POLLNVAL, RTMGRP_IPV4_ROUTE, RTMGRP_LINK,
        RTMGRP_NEIGH, RTM_DELLINK, RTM_DELNEIGH, RTM_DELROUTE, RTM_NEWLINK, RTM_NEWNEIGH,
        RTM_NEWROUTE,
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
    /// Subscribes to RTMGRP_IPV4_ROUTE | RTMGRP_NEIGH | RTMGRP_LINK multicast groups
    /// Waits for updates to arrive on the netlink socket
    /// Publishes the updated routing table every `update_interval` if needed
    pub fn start<F: FnOnce() + Send + Sync + 'static>(
        atomic_router: Arc<ArcSwap<Router>>,
        exit: Arc<AtomicBool>,
        update_interval: Duration,
        on_thread_start: F,
    ) -> thread::JoinHandle<()> {
        thread::Builder::new()
            .name("solRouteMon".to_string())
            .spawn(move || {
                // MUST remain first to run here
                on_thread_start();

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
                RTM_NEWLINK => {
                    if let Some(interface_info) = parse_rtm_ifinfomsg(m) {
                        dirty |= router.upsert_interface(interface_info);
                    }
                }
                RTM_DELLINK => {
                    if let Some(interface_info) = parse_rtm_ifinfomsg(m) {
                        dirty |= router.remove_interface(interface_info.if_index);
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
            sock: NetlinkSocket::bind((RTMGRP_IPV4_ROUTE | RTMGRP_NEIGH | RTMGRP_LINK) as u32)
                .expect("error creating netlink socket"),
            router,
            dirty: false,
            last_publish: Instant::now(),
        }
    }

    /// Resets the route monitor state by creating a new router and reinitializing
    /// the netlink socket. Used when errors occur to recover to a clean state
    fn reset(&mut self, atomic_router: &Arc<ArcSwap<Router>>) {
        let mut router = Router::new().expect("error creating Router");
        if let Err(e) = router.build_caches() {
            log::warn!("failed to build router caches on reset: {e:?}");
        }
        atomic_router.store(Arc::new(router));
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
            let mut router = self.router.clone();
            if let Err(e) = router.build_caches() {
                log::warn!("failed to build router caches before publish: {e:?}");
            }
            atomic_router.store(Arc::new(router));
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
