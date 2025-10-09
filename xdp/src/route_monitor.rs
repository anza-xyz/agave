use {
    crate::{
        netlink::{
            is_supported_ipv4_neigh_header, is_supported_ipv4_route_header, is_valid_link_update,
            parse_ifinfomsg, parse_rtm_newneigh, parse_rtm_newroute, NetlinkMessage, NetlinkSocket,
        },
        route::{AtomicRouter, WorkingRouter},
    },
    libc::{
        poll, pollfd, POLLERR, POLLHUP, POLLIN, POLLNVAL, RTMGRP_IPV4_ROUTE, RTMGRP_LINK,
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
    /// subscribes RTMGRP_IPV4_ROUTE | RTMGRP_NEIGH and drain for `drain_window`
    /// if any relevant updates arrived, update working routing table
    /// once drain window expires, publish the working routing table to the atomic router
    pub fn start(
        atomic_router: Arc<AtomicRouter>,
        exit: Arc<AtomicBool>,
        drain_window: Duration,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let groups = (RTMGRP_IPV4_ROUTE | RTMGRP_NEIGH | RTMGRP_LINK) as u32;
            let mut sock = match NetlinkSocket::open_multicast_listener(groups) {
                Ok(s) => s,
                Err(e) => {
                    error!("netlink bind failed: {e}");
                    return;
                }
            };

            let mut working = None;
            loop {
                if exit.load(Ordering::Relaxed) {
                    break;
                }
                if working.is_none() {
                    working = Some(WorkingRouter::from_atomic_router(&atomic_router));
                }

                let mut pfd = pollfd {
                    fd: sock.as_raw_fd(),
                    events: POLLIN,
                    revents: 0,
                };
                // block until data is available
                let rv = match poll_helper(&mut pfd, -1) {
                    Ok(rv) => rv,
                    Err(e) => {
                        warn!("poll error: {e}");
                        let _ = atomic_router.resync();
                        working = None;
                        continue;
                    }
                };

                // handle poll event and check for errors
                match handle_revents(rv, pfd.revents, &atomic_router, &mut working) {
                    LoopControl::Proceed => (),
                    LoopControl::Skip => continue,
                    LoopControl::Recreate => {
                        drop(sock);
                        match NetlinkSocket::open_multicast_listener(groups) {
                            Ok(s) => {
                                sock = s;
                                let _ = atomic_router.resync();
                                working = None;
                            }
                            Err(e) => {
                                error!("netlink recreate bind failed after POLLHUP: {e}");
                                return;
                            }
                        }
                        continue;
                    }
                }

                // we have data to process, so start the update window
                let start = Instant::now();
                let deadline = start.checked_add(drain_window).unwrap_or(start);

                // drain the netlink socket for the rest of the window
                loop {
                    let rv = match Self::drain_netlink_socket(
                        &sock,
                        working.as_mut().expect("working initialized"),
                        deadline,
                    ) {
                        Ok(rv) => rv,
                        Err(e) => {
                            error!("drain netlink socket failed: {e}");
                            if let Err(e) = atomic_router.resync() {
                                error!("resync failed: {e}");
                            }
                            working = None;
                            break;
                        }
                    };
                    match handle_revents(rv, pfd.revents, &atomic_router, &mut working) {
                        LoopControl::Proceed => (),
                        LoopControl::Skip => break,
                        LoopControl::Recreate => {
                            drop(sock);
                            match NetlinkSocket::open_multicast_listener(groups) {
                                Ok(s) => {
                                    sock = s;
                                    let _ = atomic_router.resync();
                                    working = None;
                                }
                                Err(e) => {
                                    error!("netlink recreate bind failed after POLLHUP: {e}");
                                    return;
                                }
                            }
                            break;
                        }
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
                    let rv = match poll_helper(&mut pfd, remain_ms) {
                        Ok(rv) => rv,
                        Err(e) => {
                            warn!("poll error during drain window wait: {e}");
                            let _ = atomic_router.resync();
                            working = None;
                            break;
                        }
                    };
                    match handle_revents(rv, pfd.revents, &atomic_router, &mut working) {
                        LoopControl::Proceed => (),
                        LoopControl::Skip => break,
                        LoopControl::Recreate => {
                            drop(sock);
                            match NetlinkSocket::open_multicast_listener(groups) {
                                Ok(s) => {
                                    sock = s;
                                    let _ = atomic_router.resync();
                                    working = None;
                                }
                                Err(e) => {
                                    error!("netlink recreate bind failed after POLLHUP: {e}");
                                    return;
                                }
                            }
                            break;
                        }
                    }
                }

                if let Some(w) = working.as_mut() {
                    if w.dirty_routes() || w.dirty_neigh() || w.dirty_interfaces() {
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
        working: &mut WorkingRouter,
        deadline: Instant,
    ) -> Result<Revents, Error> {
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
            let rv = match poll_helper(&mut pfd, 0) {
                Ok(rv) => rv,
                Err(e) => {
                    warn!("poll error during drain window wait: {e}");
                    return Err(e);
                }
            };
            match rv {
                Revents::Ready => (),
                _ => return Ok(rv),
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
                    return Err(e);
                }
            }
        }
        Ok(Revents::Ready)
    }

    #[inline]
    fn process_netlink_updates(work: &mut WorkingRouter, msgs: &[NetlinkMessage]) {
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
                RTM_NEWLINK => {
                    if is_valid_link_update(m) {
                        if let Some(interface_info) = parse_ifinfomsg(m) {
                            work.upsert_interface(interface_info);
                        }
                    }
                }
                RTM_DELLINK => {
                    if is_valid_link_update(m) {
                        if let Some(interface_info) = parse_ifinfomsg(m) {
                            work.delete_interface(interface_info.if_index);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

#[inline]
fn handle_revents(
    rv: Revents,
    revents_bits: i16,
    atomic_router: &Arc<AtomicRouter>,
    working: &mut Option<WorkingRouter>,
) -> LoopControl {
    match rv {
        Revents::Nval => {
            error!("netlink socket POLLNVAL (invalid fd), revents={revents_bits:#x}",);
            panic!("RouteMonitor: POLLNVAL on netlink socket (invalid fd)");
        }
        Revents::Hup => {
            warn!("netlink socket POLLHUP; recreating and resyncing");
            LoopControl::Recreate
        }
        Revents::Error => {
            warn!("netlink socket POLLERR; resyncing router snapshot");
            let _ = atomic_router.resync();
            *working = None;
            LoopControl::Skip
        }
        Revents::TimeoutOrNoData => {
            warn!("poll returned unexpected rc, revents={revents_bits:#x}");
            LoopControl::Skip
        }
        Revents::Ready => LoopControl::Proceed,
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Revents {
    Ready,           // POLLIN set
    TimeoutOrNoData, // no data within timeout or rc!=1
    Error,           // POLLERR
    Hup,             // POLLHUP
    Nval,            // POLLNVAL
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum LoopControl {
    Proceed,
    Skip,
    Recreate,
}

#[inline]
fn poll_helper(pfd: &mut pollfd, timeout_ms: i32) -> Result<Revents, Error> {
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
    if rc != 1 {
        // When polling a single fd, rc should be exactly 1 when not an error.
        return Ok(Revents::TimeoutOrNoData);
    }

    let re = pfd.revents;
    if (re & POLLNVAL) != 0 {
        return Ok(Revents::Nval);
    }
    if (re & POLLHUP) != 0 {
        return Ok(Revents::Hup);
    }
    if (re & POLLERR) != 0 {
        return Ok(Revents::Error);
    }
    if (re & POLLIN) != 0 {
        return Ok(Revents::Ready);
    }

    Ok(Revents::TimeoutOrNoData)
}
