use {
    crate::{
        netlink::{
            is_supported_ipv4_neigh_header, is_supported_ipv4_route_header, parse_rtm_newneigh,
            parse_rtm_newroute, NetlinkMessage, NetlinkSocket,
        },
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
    /// Batches updates for `drain_window`
    /// Once drain_window expires, publishes the working routing table to the atomic router
    /// If the netlink socket is invalid, recreates the socket and resyncs the atomic router
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
                            if ev & POLLERR != 0 { "POLLERR" } else { "" },
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
                            if !msgs.is_empty() {
                                state.dirty |=
                                    Self::process_netlink_updates(&mut state.router, &msgs);
                            }
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
                RTM_NEWROUTE if is_supported_ipv4_route_header(m) => {
                    if let Some(r) = parse_rtm_newroute(m) {
                        dirty |= router.upsert_route(r);
                    }
                }
                RTM_DELROUTE if is_supported_ipv4_route_header(m) => {
                    if let Some(r) = parse_rtm_newroute(m) {
                        dirty |= router.delete_route(r);
                    }
                }
                RTM_NEWNEIGH if is_supported_ipv4_neigh_header(m) => {
                    if let Some(n) = parse_rtm_newneigh(m, None) {
                        dirty |= router.upsert_neighbor(n);
                    }
                }
                RTM_DELNEIGH if is_supported_ipv4_neigh_header(m) => {
                    if let Some(n) = parse_rtm_newneigh(m, None) {
                        if let Some(IpAddr::V4(ip)) = n.destination {
                            dirty |= router.delete_neighbor(ip, n.ifindex);
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
    fn new(router: Router) -> Self {
        Self {
            sock: NetlinkSocket::bind((RTMGRP_IPV4_ROUTE | RTMGRP_NEIGH) as u32)
                .expect("error creating netlink socket"),
            router,
            dirty: false,
            last_publish: Instant::now(),
        }
    }

    fn reset(&mut self, atomic_router: &Arc<ArcSwap<Router>>) {
        atomic_router.store(Arc::new(Router::new().expect("error creating Router")));
        *self = Self::new(Arc::unwrap_or_clone(atomic_router.load_full()));
    }

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

#[inline]
fn poll(pfd: &mut pollfd, timeout: Duration) -> Result<i32, Error> {
    let rc = loop {
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
