use {
    crate::{
        netlink::{
            is_supported_ipv4_neigh_header, is_supported_ipv4_route_header, parse_rtm_newneigh,
            parse_rtm_newroute, NetlinkMessage, NetlinkSocket,
        },
        route::{AtomicRouter, Working},
    },
    libc::{
        poll, pollfd, POLLIN, RTMGRP_IPV4_ROUTE, RTMGRP_NEIGH, RTM_DELNEIGH, RTM_DELROUTE,
        RTM_NEWNEIGH, RTM_NEWROUTE,
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
    /// subscribes RTMGRP_IPV4_ROUTE | RTMGRP_NEIGH and drain for `drain_window`
    /// if any relevant updates arrived, update working routing table
    /// once drain window expires, publish the working routing table to the atomic router
    pub fn start(
        atomic_router: Arc<AtomicRouter>,
        exit: Arc<AtomicBool>,
        drain_window: Duration,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let groups = (RTMGRP_IPV4_ROUTE | RTMGRP_NEIGH) as u32;
            let sock = match NetlinkSocket::open_multicast_listener(groups) {
                Ok(s) => s,
                Err(e) => {
                    log::error!("netlink bind failed: {e}");
                    return;
                }
            };

            let mut working = None;
            loop {
                if exit.load(Ordering::Relaxed) {
                    break;
                }
                if working.is_none() {
                    working = Some(Working::from_atomic_router(&atomic_router));
                }

                let mut pfd = pollfd {
                    fd: sock.as_raw_fd(),
                    events: POLLIN,
                    revents: 0,
                };
                let rc = loop {
                    let rc = unsafe { poll(&mut pfd as *mut pollfd, 1, -1) };
                    if rc < 0 && Error::last_os_error().kind() == ErrorKind::Interrupted {
                        continue;
                    }
                    break rc;
                };
                if rc < 0 {
                    warn!("poll error: {}", Error::last_os_error());
                    let _ = atomic_router.resync();
                    working = None;
                    continue;
                }
                if (pfd.revents & POLLIN) == 0 {
                    continue;
                }

                // coalesce update window
                let start = Instant::now();
                let deadline = start.checked_add(drain_window).unwrap_or(start);

                // drain the netlink socket for the rest of the window
                loop {
                    if !Self::drain_netlink_socket(
                        &sock,
                        working.as_mut().expect("working initialized"),
                        deadline,
                    ) {
                        if let Err(e) = atomic_router.resync() {
                            error!("resync failed: {e}");
                        }
                        working = None;
                        break;
                    }

                    let now = Instant::now();
                    if now >= deadline {
                        break;
                    }

                    // in this case, we still have time left, so we wait for more updates
                    let remain_ms = deadline
                        .checked_duration_since(now)
                        .unwrap_or_default()
                        .as_millis() as i32;
                    pfd.revents = 0;
                    let rc = loop {
                        let rc = unsafe { poll(&mut pfd as *mut pollfd, 1, remain_ms) };
                        if rc < 0 && Error::last_os_error().kind() == ErrorKind::Interrupted {
                            continue;
                        }
                        break rc;
                    };
                    if rc <= 0 || (pfd.revents & POLLIN) == 0 {
                        break;
                    }
                }

                if let Some(w) = working.as_mut() {
                    if w.dirty_routes() || w.dirty_neigh() {
                        atomic_router.publish_snapshot(w);
                        w.clear_dirty();
                    }
                }
            }
        })
    }

    #[inline]
    fn drain_netlink_socket(
        sock: &NetlinkSocket,
        working: &mut Working,
        deadline: Instant,
    ) -> bool {
        let mut pfd = pollfd {
            fd: sock.as_raw_fd(),
            events: POLLIN,
            revents: 0,
        };
        loop {
            if Instant::now() >= deadline {
                break;
            }

            pfd.revents = 0;
            let rc0 = unsafe { poll(&mut pfd as *mut pollfd, 1, 0) };
            if rc0 <= 0 || (pfd.revents & POLLIN) == 0 {
                break;
            }

            match sock.recv() {
                Ok(msgs) => {
                    if msgs.is_empty() {
                        break;
                    }
                    Self::process_netlink_updates(working, &msgs);
                }
                Err(e) => {
                    warn!("recv during drain failed: {e}");
                    return false;
                }
            }
        }
        true
    }

    #[inline]
    fn process_netlink_updates(work: &mut Working, msgs: &[NetlinkMessage]) {
        for m in msgs {
            match m.header.nlmsg_type {
                RTM_NEWROUTE if is_supported_ipv4_route_header(m) => {
                    if let Some(r) = parse_rtm_newroute(m) {
                        work.upsert_route(r);
                    }
                }
                RTM_DELROUTE if is_supported_ipv4_route_header(m) => {
                    if let Some(r) = parse_rtm_newroute(m) {
                        work.delete_route(r);
                    }
                }
                RTM_NEWNEIGH if is_supported_ipv4_neigh_header(m) => {
                    if let Some(n) = parse_rtm_newneigh(m, None) {
                        work.upsert_neighbor(n);
                    }
                }
                RTM_DELNEIGH if is_supported_ipv4_neigh_header(m) => {
                    if let Some(n) = parse_rtm_newneigh(m, None) {
                        if let Some(IpAddr::V4(ip)) = n.destination {
                            work.delete_neighbor(ip, n.ifindex);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}
