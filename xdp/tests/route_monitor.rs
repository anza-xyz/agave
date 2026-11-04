#![cfg(target_os = "linux")]

mod common;

use {
    agave_xdp::{
        netlink::{MacAddress, netlink_get_neighbors},
        route::{RouteError, RouteTable, Router},
        route_monitor::RouteMonitor,
    },
    arc_swap::ArcSwap,
    common::NudState,
    libc::{AF_INET, NUD_REACHABLE, NUD_STALE},
    std::{
        net::{IpAddr, Ipv4Addr},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::JoinHandle,
        time::Duration,
    },
};

struct RouteMonitorGuard {
    router: Arc<ArcSwap<Router>>,
    exit: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for RouteMonitorGuard {
    fn drop(&mut self) {
        self.exit.store(true, Ordering::Relaxed);
        let Some(handle) = self.handle.take() else {
            return;
        };
        if let Err(err) = handle.join() {
            if std::thread::panicking() {
                eprintln!("route monitor thread panicked: {err:?}");
            } else {
                std::panic::resume_unwind(err);
            }
        }
    }
}

fn start_route_monitor() -> RouteMonitorGuard {
    let router = Router::new().expect("build initial router");
    let router = Arc::new(ArcSwap::from_pointee(router));
    let exit = Arc::new(AtomicBool::new(false));
    let handle = RouteMonitor::start(
        Arc::clone(&router),
        RouteTable::Main,
        Arc::clone(&exit),
        Duration::ZERO,
        || {},
    );
    RouteMonitorGuard {
        router,
        exit,
        handle: Some(handle),
    }
}

#[test]
#[ignore = "requires root and network namespace privileges"]
fn route_monitor_publishes_live_route_updates() {
    let _netns = common::NetNsGuard::new().expect("create network namespace");
    let links = common::setup_veth_pair_with_tx_queue_count(1);

    let monitor = start_route_monitor();

    let routed_destination = Ipv4Addr::new(203, 0, 113, 7);
    assert!(matches!(
        monitor.router.load().route_v4(routed_destination),
        Err(RouteError::NoRouteFound(_))
    ));

    common::replace_neighbor(
        links.right_ip,
        links.right_mac,
        common::LEFT_IFACE,
        NudState::Permanent,
    );
    common::add_route("203.0.113.0/24", links.right_ip, common::LEFT_IFACE);

    common::wait_until(
        "the route monitor to publish a newly added route",
        Duration::from_secs(2),
        || {
            let router = monitor.router.load();
            match router.route_v4(routed_destination) {
                Ok(next_hop)
                    if next_hop.if_index == links.left_if_index
                        && next_hop.ip_addr == IpAddr::V4(links.right_ip)
                        && next_hop.mac_addr == Some(links.right_mac) =>
                {
                    Some(())
                }
                _ => None,
            }
        },
    );

    common::delete_route("203.0.113.0/24");
    common::wait_until(
        "the route monitor to publish a removed route",
        Duration::from_secs(2),
        || {
            matches!(
                monitor.router.load().route_v4(routed_destination),
                Err(RouteError::NoRouteFound(_))
            )
            .then_some(())
        },
    );
}

#[test]
#[ignore = "requires root and network namespace privileges"]
fn route_monitor_publishes_live_neighbor_updates() {
    let _netns = common::NetNsGuard::new().expect("create network namespace");
    let links = common::setup_veth_pair_with_tx_queue_count(1);

    let monitor = start_route_monitor();
    let routed_destination = Ipv4Addr::new(203, 0, 113, 7);

    common::add_route("203.0.113.0/24", links.right_ip, common::LEFT_IFACE);
    let initial_mac = links.right_mac;
    common::replace_neighbor(
        links.right_ip,
        initial_mac,
        common::LEFT_IFACE,
        NudState::Permanent,
    );

    common::wait_until(
        "the route monitor to publish the initial neighbor",
        Duration::from_secs(2),
        || {
            let router = monitor.router.load();
            match router.route_v4(routed_destination) {
                Ok(next_hop)
                    if next_hop.if_index == links.left_if_index
                        && next_hop.ip_addr == IpAddr::V4(links.right_ip)
                        && next_hop.mac_addr == Some(initial_mac) =>
                {
                    Some(())
                }
                _ => None,
            }
        },
    );

    let updated_mac = MacAddress([0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0x44]);
    common::replace_neighbor(
        links.right_ip,
        updated_mac,
        common::LEFT_IFACE,
        NudState::Permanent,
    );

    common::wait_until(
        "the route monitor to publish a replaced neighbor",
        Duration::from_secs(2),
        || {
            let router = monitor.router.load();
            match router.route_v4(routed_destination) {
                Ok(next_hop) if next_hop.mac_addr == Some(updated_mac) => Some(()),
                _ => None,
            }
        },
    );

    common::delete_neighbor(links.right_ip, common::LEFT_IFACE);
    common::wait_until(
        "the route monitor to publish a removed neighbor",
        Duration::from_secs(2),
        || {
            let router = monitor.router.load();
            match router.route_v4(routed_destination) {
                Ok(next_hop)
                    if next_hop.if_index == links.left_if_index
                        && next_hop.ip_addr == IpAddr::V4(links.right_ip)
                        && next_hop.mac_addr.is_none() =>
                {
                    Some(())
                }
                _ => None,
            }
        },
    );
}

#[test]
#[ignore = "requires root and network namespace privileges"]
fn route_monitor_publishes_link_removals() {
    let _netns = common::NetNsGuard::new().expect("create network namespace");
    let links = common::setup_veth_pair_with_tx_queue_count(1);

    common::replace_neighbor(
        links.right_ip,
        links.right_mac,
        common::LEFT_IFACE,
        NudState::Permanent,
    );
    common::add_route("203.0.113.0/24", links.right_ip, common::LEFT_IFACE);

    let monitor = start_route_monitor();
    let routed_destination = Ipv4Addr::new(203, 0, 113, 7);
    common::wait_until(
        "the route monitor to publish the initial link-backed route",
        Duration::from_secs(2),
        || {
            let router = monitor.router.load();
            match router.route_v4(routed_destination) {
                Ok(next_hop)
                    if next_hop.if_index == links.left_if_index
                        && next_hop.ip_addr == IpAddr::V4(links.right_ip) =>
                {
                    Some(())
                }
                _ => None,
            }
        },
    );

    common::delete_link(common::LEFT_IFACE);
    common::wait_until(
        "the route monitor to publish a removed link",
        Duration::from_secs(2),
        || {
            matches!(
                monitor.router.load().route_v4(routed_destination),
                Err(RouteError::NoRouteFound(_))
            )
            .then_some(())
        },
    );
}

#[test]
#[ignore = "requires root and network namespace privileges"]
fn route_monitor_publishes_live_gre_route_updates() {
    let _netns = common::NetNsGuard::new().expect("create network namespace");
    let links = common::setup_veth_pair_with_tx_queue_count(1);

    let monitor = start_route_monitor();
    let overlay_destination = Ipv4Addr::new(192, 0, 2, 99);
    assert!(matches!(
        monitor.router.load().route_v4(overlay_destination),
        Err(RouteError::NoRouteFound(_))
    ));

    common::replace_neighbor(
        links.right_ip,
        links.right_mac,
        common::LEFT_IFACE,
        NudState::Permanent,
    );
    common::add_route_to_dev(&format!("{}/32", links.right_ip), common::LEFT_IFACE);
    let gre = common::setup_gre_tunnel(&links);
    common::add_route_to_dev_with_src("192.0.2.0/24", common::GRE_IFACE, gre.overlay_ip);

    common::wait_until(
        "the route monitor to publish a GRE overlay route",
        Duration::from_secs(2),
        || {
            let router = monitor.router.load();
            match router.route_v4(overlay_destination) {
                Ok(next_hop)
                    if next_hop.if_index == gre.if_index
                        && next_hop.ip_addr == IpAddr::V4(overlay_destination)
                        && next_hop.mac_addr == Some(links.right_mac)
                        && next_hop.preferred_src_ip == Some(gre.overlay_ip)
                        && next_hop.gre.as_ref().is_some_and(|gre_route| {
                            gre_route.if_index == gre.if_index
                                && gre_route.underlay_mac_addr == Some(links.right_mac)
                                && gre_route.tunnel_info.local == IpAddr::V4(gre.local_ip)
                                && gre_route.tunnel_info.remote == IpAddr::V4(gre.remote_ip)
                        }) =>
                {
                    Some(())
                }
                _ => None,
            }
        },
    );

    common::delete_link(common::GRE_IFACE);
    common::wait_until(
        "the route monitor to publish a removed GRE link",
        Duration::from_secs(2),
        || {
            matches!(
                monitor.router.load().route_v4(overlay_destination),
                Err(RouteError::NoRouteFound(_))
            )
            .then_some(())
        },
    );
}

/// Test that if a gateway becomes STALE, we don't discard its known MAC address or break
/// default-route lookup.
#[test]
#[ignore = "requires root and network namespace privileges"]
fn route_monitor_keeps_stale_default_gateway_neighbor() {
    let _netns = common::NetNsGuard::new().expect("create network namespace");
    let links = common::setup_veth_pair_with_tx_queue_count(1);

    common::replace_neighbor(
        links.right_ip,
        links.right_mac,
        common::LEFT_IFACE,
        NudState::Reachable,
    );
    let monitor = start_route_monitor();
    common::add_route("default", links.right_ip, common::LEFT_IFACE);

    let routed_destination = Ipv4Addr::new(203, 0, 113, 7);
    let reachable_router = common::wait_until(
        "the route monitor to publish the default route through a reachable gateway",
        Duration::from_secs(2),
        || {
            let router = monitor.router.load_full();
            let next_hop = router.route_v4(routed_destination).ok()?;
            (next_hop.if_index == links.left_if_index
                && next_hop.ip_addr == IpAddr::V4(links.right_ip)
                && next_hop.mac_addr == Some(links.right_mac))
            .then_some(router)
        },
    );

    let gateway_neighbor = || {
        netlink_get_neighbors(None, AF_INET as u8)
            .expect("read the gateway neighbor from netlink")
            .into_iter()
            .find(|neighbor| {
                neighbor.ifindex == links.left_if_index as i32
                    && neighbor.destination == Some(IpAddr::V4(links.right_ip))
            })
            .expect("the gateway neighbor should remain in the kernel neighbor table")
    };
    assert_eq!(
        gateway_neighbor().state,
        NUD_REACHABLE,
        "the gateway should start REACHABLE so the test exercises a transition to STALE"
    );

    common::replace_neighbor(
        links.right_ip,
        links.right_mac,
        common::LEFT_IFACE,
        NudState::Stale,
    );
    let stale_neighbor = gateway_neighbor();
    assert_eq!(
        stale_neighbor.state, NUD_STALE,
        "the kernel gateway neighbor should become STALE to exercise the netlink update path"
    );
    assert_eq!(
        stale_neighbor.lladdr,
        Some(links.right_mac),
        "becoming STALE must preserve the gateway's cached MAC address"
    );

    // Both REACHABLE and STALE neighbors require refresh, so next-hop values alone cannot
    // distinguish the old router from one rebuilt after the STALE notification.
    let stale_router = common::wait_until(
        "the route monitor to publish a new router after the gateway becomes STALE",
        Duration::from_secs(2),
        || {
            let router = monitor.router.load_full();
            (!Arc::ptr_eq(&router, &reachable_router)).then_some(router)
        },
    );

    for next_hop in [
        stale_router
            .default()
            .expect("a stale gateway should not remove the cached default route"),
        stale_router
            .route_v4(routed_destination)
            .expect("off-subnet traffic should remain routable through a stale gateway"),
    ] {
        assert_eq!(
            next_hop.if_index, links.left_if_index,
            "a stale gateway should not change the default route's output interface"
        );
        assert_eq!(
            next_hop.ip_addr,
            IpAddr::V4(links.right_ip),
            "the default route should still forward traffic through its configured gateway"
        );
        assert_eq!(
            next_hop.mac_addr,
            Some(links.right_mac),
            "a STALE neighbor still has a usable MAC and should not be removed from the router"
        );
        assert!(
            next_hop.neigh_requires_refresh,
            "the stale gateway should request refresh while its cached MAC remains usable"
        );
    }
}
