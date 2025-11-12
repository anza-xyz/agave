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
        poll, pollfd, POLLERR, POLLHUP, POLLIN, POLLNVAL, RTMGRP_IPV4_ROUTE, RTMGRP_NEIGH,
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
        drain_window: Duration,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let groups = (RTMGRP_IPV4_ROUTE | RTMGRP_NEIGH) as u32;
            let mut sock = match NetlinkSocket::bind(groups) {
                Ok(s) => s,
                Err(e) => {
                    error!("netlink bind failed: {e}");
                    return;
                }
            };

            let mut working: Option<Router> = None;
            let mut dirty = false;
            let mut in_window = false;
            let mut deadline = Instant::now();
            loop {
                if exit.load(Ordering::Relaxed) {
                    break;
                }
                if working.is_none() {
                    let snapshot = atomic_router.load();
                    working = Some((**snapshot).clone());
                }

                // calculate timeout.
                // - block forever if not in window
                // - block until deadline
                let timeout_ms = if in_window {
                    let now = Instant::now();
                    if now >= deadline {
                        publish_if_dirty(&atomic_router, &working, &mut dirty);
                        in_window = false;
                        -1
                    } else {
                        deadline.saturating_duration_since(now).as_millis() as i32
                    }
                } else {
                    -1
                };

                // Poll
                let mut pfd = pollfd {
                    fd: sock.as_raw_fd(),
                    events: POLLIN,
                    revents: 0,
                };
                let rc = match poll_helper(&mut pfd, timeout_ms) {
                    Ok(rc) => rc,
                    Err(e) => {
                        error!("poll error: {e}");
                        if let Err(()) = recreate_listener(groups, &atomic_router).map(|s| sock = s)
                        {
                            return;
                        }
                        working = None;
                        in_window = false;
                        dirty = false;
                        continue;
                    }
                };

                // Timeout. Publish snapshot if dirty at top of loop.
                if rc == 0 {
                    continue;
                }

                let re = pfd.revents;

                if (re & POLLNVAL) != 0 {
                    error!(
                        "netlink socket POLLNVAL; invalid fd; recreating and resyncing \
                         (revents={re:#x})"
                    );
                    if let Err(()) = recreate_listener(groups, &atomic_router).map(|s| sock = s) {
                        return;
                    }
                    working = None;
                    in_window = false;
                    dirty = false;
                    continue;
                }

                if (re & (POLLHUP | POLLERR)) != 0 {
                    warn!(
                        "netlink socket {:?}; recreating and resyncing (revents={:#x})",
                        if (re & POLLERR) != 0 {
                            "POLLERR"
                        } else {
                            "POLLHUP"
                        },
                        re
                    );
                    if let Err(()) = recreate_listener(groups, &atomic_router).map(|s| sock = s) {
                        return;
                    }
                    working = None;
                    in_window = false;
                    dirty = false;
                    continue;
                }
                if (re & POLLIN) == 0 {
                    // Unhandled - ignore
                    debug!("poll returned without POLLIN, revents={re:#x}");
                    continue;
                }
                // Drain channel
                match sock.recv() {
                    Ok(msgs) => {
                        if !msgs.is_empty() {
                            Self::process_netlink_updates(
                                working
                                    .as_mut()
                                    .expect("working router should be initialized"),
                                &mut dirty,
                                &msgs,
                            );
                            if !in_window {
                                let start = Instant::now();
                                deadline = start.checked_add(drain_window).unwrap_or(start);
                                in_window = true;
                            }
                        }
                    }
                    Err(e) => {
                        warn!("recv failed: {e}");
                        if let Err(()) = recreate_listener(groups, &atomic_router).map(|s| sock = s)
                        {
                            return;
                        }
                        working = None;
                        in_window = false;
                        dirty = false;
                        continue;
                    }
                }
            }
        })
    }

    #[inline]
    fn process_netlink_updates(working: &mut Router, dirty: &mut bool, msgs: &[NetlinkMessage]) {
        for m in msgs {
            match m.header.nlmsg_type {
                RTM_NEWROUTE if is_supported_ipv4_route_header(m) => {
                    if let Some(r) = parse_rtm_newroute(m) {
                        *dirty |= working.upsert_route(r);
                    }
                }
                RTM_DELROUTE if is_supported_ipv4_route_header(m) => {
                    if let Some(r) = parse_rtm_newroute(m) {
                        *dirty |= working.delete_route(r);
                    }
                }
                RTM_NEWNEIGH if is_supported_ipv4_neigh_header(m) => {
                    if let Some(n) = parse_rtm_newneigh(m, None) {
                        *dirty |= working.upsert_neighbor(n);
                    }
                }
                RTM_DELNEIGH if is_supported_ipv4_neigh_header(m) => {
                    if let Some(n) = parse_rtm_newneigh(m, None) {
                        if let Some(IpAddr::V4(ip)) = n.destination {
                            *dirty |= working.delete_neighbor(ip, n.ifindex);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

#[inline]
fn poll_helper(pfd: &mut pollfd, timeout_ms: i32) -> Result<i32, Error> {
    let rc = loop {
        let rc = unsafe { poll(pfd as *mut pollfd, 1, timeout_ms) };
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

#[inline]
fn publish_if_dirty(
    atomic_router: &Arc<ArcSwap<Router>>,
    working: &Option<Router>,
    dirty: &mut bool,
) {
    if *dirty {
        if let Some(w) = working.as_ref() {
            atomic_router.store(Arc::new(w.clone()));
        }
        *dirty = false;
    }
}

#[inline]
fn recreate_listener(
    groups: u32,
    atomic_router: &Arc<ArcSwap<Router>>,
) -> Result<NetlinkSocket, ()> {
    match NetlinkSocket::bind(groups) {
        Ok(s) => {
            if let Ok(r) = Router::new() {
                atomic_router.store(Arc::new(r));
            }
            Ok(s)
        }
        Err(e) => {
            error!("netlink recreate bind failed: {e}");
            Err(())
        }
    }
}
