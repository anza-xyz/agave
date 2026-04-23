#![cfg(target_os = "linux")]

mod common;

use {
    agave_xdp::{
        route::{RouteError, RouteTable, Router},
        route_monitor::RouteMonitor,
    },
    arc_swap::ArcSwap,
    std::{
        net::{IpAddr, Ipv4Addr},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    },
};

#[test]
fn route_monitor_publishes_live_route_updates() {
    let _netns = common::NetNsGuard::new();
    let links = common::setup_veth_pair();

    let router = Router::new().expect("build initial router");
    let atomic_router = Arc::new(ArcSwap::from_pointee(router));
    let exit = Arc::new(AtomicBool::new(false));
    let handle = RouteMonitor::start(
        Arc::clone(&atomic_router),
        RouteTable::Main,
        Arc::clone(&exit),
        Duration::ZERO,
        || {},
    );

    let routed_destination = Ipv4Addr::new(203, 0, 113, 7);
    assert!(matches!(
        atomic_router.load().route_v4(routed_destination),
        Err(RouteError::NoRouteFound(_))
    ));

    common::replace_neighbor(links.right_ip, links.right_mac, links.left_name);
    common::add_route("203.0.113.0/24", links.right_ip, links.left_name);

    let next_hop = common::wait_until(
        "the route monitor to publish a newly added route",
        Duration::from_secs(2),
        || {
            let router = atomic_router.load();
            match router.route_v4(routed_destination) {
                Ok(next_hop)
                    if next_hop.if_index == links.left_if_index
                        && next_hop.ip_addr == IpAddr::V4(links.right_ip)
                        && next_hop.mac_addr == Some(links.right_mac) =>
                {
                    Some(next_hop)
                }
                _ => None,
            }
        },
    );
    assert_eq!(next_hop.if_index, links.left_if_index);
    assert_eq!(next_hop.ip_addr, IpAddr::V4(links.right_ip));
    assert_eq!(next_hop.mac_addr, Some(links.right_mac));

    common::delete_route("203.0.113.0/24");
    common::wait_until(
        "the route monitor to publish a removed route",
        Duration::from_secs(2),
        || {
            matches!(
                atomic_router.load().route_v4(routed_destination),
                Err(RouteError::NoRouteFound(_))
            )
            .then_some(())
        },
    );

    exit.store(true, Ordering::Relaxed);
    handle.join().expect("join route monitor thread");
}
