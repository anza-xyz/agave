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
    /// Drains the netlink socket for `drain_window`
    /// If any relevant updates arrive, update working routing table
    /// Once drain window expires, publish the working routing table to the atomic router
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
                    error!("greg: netlink bind failed: {e}");
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

                // compute timeout: block forever if not in a window; otherwise wait until deadline
                let timeout_ms = if in_window {
                    let now = Instant::now();
                    if now >= deadline {
                        publish_if_dirty(&atomic_router, &working, &mut dirty);
                        in_window = false;
                        -1
                    } else {
                        deadline
                            .checked_duration_since(now)
                            .unwrap_or_default()
                            .as_millis() as i32
                    }
                } else {
                    -1
                };

                let mut pfd = pollfd {
                    fd: sock.as_raw_fd(),
                    events: POLLIN,
                    revents: 0,
                };
                if let Err(e) = poll_nointr(&mut pfd, timeout_ms) {
                    error!("greg: poll error: {e}");
                    resync_router(&atomic_router);
                    working = None;
                    continue;
                }

                let re = pfd.revents;
                if re == 0 {
                    // timeout or no new data (only occurs when in_window was true)
                    if in_window {
                        publish_if_dirty(&atomic_router, &working, &mut dirty);
                        in_window = false;
                    }
                    continue;
                }

                if (re & POLLNVAL) != 0 {
                    panic!(
                        "greg: RouteMonitor: POLLNVAL on netlink socket (invalid fd). \
                         revents={re:#x}"
                    );
                }
                if (re & POLLHUP) != 0 {
                    warn!("greg: netlink socket POLLHUP; recreating and resyncing");
                    drop(sock);
                    match recreate_listener(groups, &atomic_router) {
                        Ok(s) => {
                            sock = s;
                            working = None;
                            in_window = false;
                            dirty = false;
                        }
                        Err(()) => return,
                    }
                    continue;
                }
                if (re & POLLERR) != 0 {
                    warn!("greg: netlink socket POLLERR; resyncing router snapshot");
                    resync_router(&atomic_router);
                    working = None;
                    in_window = false;
                    continue;
                }
                if (re & POLLIN) == 0 {
                    // unexpected/unhandled events (e.g. POLLPRI) or spurious wake
                    info!("greg: poll returned without POLLIN, revents={re:#x}"); //greg: convert to debug
                    continue;
                }

                // read netlink messages, update working router if dirty, update read window
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
                        warn!("greg: recv failed: {e}");
                        drop(sock);
                        match recreate_listener(groups, &atomic_router) {
                            Ok(s) => {
                                sock = s;
                                working = None;
                                in_window = false;
                                dirty = false;
                            }
                            Err(()) => return,
                        }
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
                        info!("greg: upsert_route");
                        *dirty |= working.upsert_route(r);
                    }
                }
                RTM_DELROUTE if is_supported_ipv4_route_header(m) => {
                    if let Some(r) = parse_rtm_newroute(m) {
                        info!("greg: delete_route");
                        *dirty |= working.delete_route(r);
                    }
                }
                RTM_NEWNEIGH if is_supported_ipv4_neigh_header(m) => {
                    if let Some(n) = parse_rtm_newneigh(m, None) {
                        info!("greg: upsert_neighbor");
                        *dirty |= working.upsert_neighbor(n);
                    }
                }
                RTM_DELNEIGH if is_supported_ipv4_neigh_header(m) => {
                    if let Some(n) = parse_rtm_newneigh(m, None) {
                        info!("greg: delete_neighbor");
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
fn poll_nointr(pfd: &mut pollfd, timeout_ms: i32) -> Result<(), Error> {
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
    Ok(())
}

#[inline]
fn resync_router(atomic_router: &Arc<ArcSwap<Router>>) {
    if let Ok(r) = Router::new() {
        atomic_router.store(Arc::new(r));
    }
}

#[inline]
fn publish_if_dirty(
    atomic_router: &Arc<ArcSwap<Router>>,
    working: &Option<Router>,
    dirty: &mut bool,
) {
    info!("greg: publish_if_dirty: dirty={dirty}");
    if *dirty {
        if let Some(w) = working.as_ref() {
            info!("greg: publish_if_dirty: storing router");
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
            resync_router(atomic_router);
            Ok(s)
        }
        Err(e) => {
            error!("netlink recreate bind failed: {e}");
            Err(())
        }
    }
}
