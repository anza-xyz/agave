use {
    crate::{
        netlink::{NetlinkMessage, NetlinkSocket, parse_rtm_newneigh, parse_rtm_newroute},
        route::{RouteTable, Router, RoutingTables},
    },
    arc_swap::ArcSwap,
    libc::{
        self, POLLERR, POLLHUP, POLLIN, POLLNVAL, RTM_DELLINK, RTM_DELNEIGH, RTM_DELROUTE,
        RTM_NEWLINK, RTM_NEWNEIGH, RTM_NEWROUTE, RTMGRP_IPV4_ROUTE, RTMGRP_LINK, RTMGRP_NEIGH,
        pollfd,
    },
    log::*,
    std::{
        io::{Error, ErrorKind},
        net::IpAddr,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
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
        route_table: RouteTable,
        exit: Arc<AtomicBool>,
        update_interval: Duration,
        on_thread_start: F,
    ) -> thread::JoinHandle<()> {
        thread::Builder::new()
            .name("solRouteMon".to_string())
            .spawn(move || {
                // MUST remain first to run here
                on_thread_start();

                let mut state = RouteMonitorState::new(route_table);

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
                        // we get POLLERR if the socket overflows
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
                    // drain the socket
                    loop {
                        match state.sock.recv_nonblocking() {
                            Ok(Some(msgs)) => {
                                if msgs.is_empty() {
                                    warn!("netlink recv returned empty message list");
                                    continue;
                                }
                                state.update(&msgs);
                            }
                            Ok(None) => break,
                            Err(e) => {
                                // we get here if recv() catches ENOBUFS or if the returned buffer
                                // exceeds NLMSG_GOODSIZE
                                error!("netlink recv error: {e}");
                                state.reset(&atomic_router);
                                break;
                            }
                        }
                    }
                }
            })
            .unwrap()
    }
}

struct RouteMonitorState {
    sock: NetlinkSocket,
    route_table: RouteTable,
    pending_events: PendingEvents,
    last_publish: Instant,
}

#[derive(Default)]
struct PendingEvents {
    routes: usize,
    neighbors: usize,
    links: usize,
    errors: usize,
}

impl PendingEvents {
    fn is_empty(&self) -> bool {
        self.routes == 0 && self.neighbors == 0 && self.links == 0 && self.errors == 0
    }
}

impl RouteMonitorState {
    /// Creates a new RouteMonitorState with a bounded netlink socket
    fn new(route_table: RouteTable) -> Self {
        Self {
            sock: bind_socket(),
            route_table,
            pending_events: PendingEvents::default(),
            last_publish: Instant::now(),
        }
    }

    #[inline]
    fn update(&mut self, msgs: &[NetlinkMessage]) {
        for message in msgs {
            match message.header.nlmsg_type {
                RTM_NEWROUTE | RTM_DELROUTE => {
                    let Some(route) = parse_rtm_newroute(message) else {
                        continue;
                    };
                    if !route
                        .table
                        .is_some_and(|table| self.route_table == table.into())
                    {
                        continue;
                    }
                    self.pending_events.routes = self.pending_events.routes.saturating_add(1);
                    debug!(
                        "route monitor update {} table {} dst={:?}/{} gateway={:?} oif={:?} \
                         priority={:?}",
                        nlmsg_type_name(message.header.nlmsg_type),
                        self.route_table,
                        route.destination,
                        route.dst_len,
                        route.gateway,
                        route.out_if_index,
                        route.priority,
                    );
                }
                RTM_NEWNEIGH | RTM_DELNEIGH => {
                    let Some(neighbor) = parse_rtm_newneigh(message, None) else {
                        continue;
                    };
                    if !matches!(neighbor.destination, Some(IpAddr::V4(_))) {
                        continue;
                    }
                    self.pending_events.neighbors = self.pending_events.neighbors.saturating_add(1);
                    debug!(
                        "route monitor update {} neighbor={:?} ifindex={} state={} lladdr={:?}",
                        nlmsg_type_name(message.header.nlmsg_type),
                        neighbor.destination,
                        neighbor.ifindex,
                        neighbor.state,
                        neighbor.lladdr,
                    );
                }
                RTM_NEWLINK | RTM_DELLINK => {
                    self.pending_events.links = self.pending_events.links.saturating_add(1);
                    debug!(
                        "route monitor update {}",
                        nlmsg_type_name(message.header.nlmsg_type)
                    );
                }
                _ => {}
            }
        }
    }

    /// Resets the route monitor state by creating a new router and reinitializing
    /// the netlink socket.
    fn reset(&mut self, atomic_router: &Arc<ArcSwap<Router>>) {
        // the most likely (albeit uncommon) way to get here is a huge burst of incoming
        // notifications that causes the netlink socket to overflow. When that happens poll/recv
        // return POLLERR/ENOBUFS, we detect that and recover by reloading the socket and the
        // entire routing state.
        self.sock = bind_socket();
        self.pending_events.errors = self.pending_events.errors.saturating_add(1);
        log_router_rebuild(self.route_table, &self.pending_events);
        let router = match rebuild_router(self.route_table) {
            Ok(router) => router,
            Err(e) => {
                // If we fail to rebuild the router (unlikely but possible if route updates keep
                // coming for more than 3s - see rebuild_router()), we don't reset
                // self.pending_events so that we attempt to rebuild again on the next publish
                // interval.
                //
                // We don't update self.last_publish as rebuild_router() will sleep between retries
                // so there's no risk of getting in a tight retry/publish loop.
                warn!("failed to rebuild router from netlink during reset: {e}");
                return;
            }
        };
        log_router_publish(self.route_table, &router);
        atomic_router.store(Arc::new(router));
        self.pending_events = PendingEvents::default();
        self.last_publish = Instant::now();
    }

    /// Publishes the updated router if there are new route/neighbor updates
    /// and the update interval has elapsed
    fn publish_if_needed(
        &mut self,
        atomic_router: &Arc<ArcSwap<Router>>,
        update_interval: Duration,
    ) {
        if !self.pending_events.is_empty() && self.last_publish.elapsed() >= update_interval {
            log_router_rebuild(self.route_table, &self.pending_events);
            match rebuild_router(self.route_table) {
                Ok(router) => {
                    log_router_publish(self.route_table, &router);
                    atomic_router.store(Arc::new(router));
                    self.pending_events = PendingEvents::default();
                }
                Err(e) => warn!("failed to rebuild router from netlink: {e}"),
            }
            self.last_publish = Instant::now();
        }
    }
}

fn log_router_publish(route_table: RouteTable, router: &Router) {
    debug!(
        "published router table {route_table}:\n{}",
        router.routing_table()
    );
}

fn log_router_rebuild(route_table: RouteTable, pending_rebuild: &PendingEvents) {
    info!(
        "rebuilding router table {route_table}: route_events={} neighbor_events={} link_events={} \
         error_events={}",
        pending_rebuild.routes,
        pending_rebuild.neighbors,
        pending_rebuild.links,
        pending_rebuild.errors,
    );
}

fn nlmsg_type_name(nlmsg_type: u16) -> &'static str {
    match nlmsg_type {
        RTM_NEWROUTE => "RTM_NEWROUTE",
        RTM_DELROUTE => "RTM_DELROUTE",
        RTM_NEWNEIGH => "RTM_NEWNEIGH",
        RTM_DELNEIGH => "RTM_DELNEIGH",
        RTM_NEWLINK => "RTM_NEWLINK",
        RTM_DELLINK => "RTM_DELLINK",
        _ => "RTM_UNKNOWN",
    }
}

fn bind_socket() -> NetlinkSocket {
    NetlinkSocket::bind((RTMGRP_IPV4_ROUTE | RTMGRP_NEIGH | RTMGRP_LINK) as u32)
        // this should never fail unless there's a configuration bug (eg no perms)
        .expect("failed to bind netlink socket")
}

fn rebuild_router(route_table: RouteTable) -> Result<Router, Error> {
    let mut retries = 0u8;
    loop {
        if retries == 10 {
            return Err(Error::new(
                ErrorKind::Interrupted,
                "failed to build routing table after 10 attempts",
            ));
        }

        match RoutingTables::from_netlink(route_table) {
            Ok(tables) => return Router::from_tables(tables),
            Err(e) if e.kind() == ErrorKind::Interrupted => {
                warn!("interrupted while building routing table, retrying");
                thread::sleep(Duration::from_secs(1));
                retries = retries.saturating_add(1);
            }
            Err(e) => return Err(e),
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
