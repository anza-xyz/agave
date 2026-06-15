#![cfg(target_os = "linux")]

mod common;

use {
    agave_xdp::{
        netlink::MacAddress,
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
    let _netns = common::NetNsGuard::new();
    let links = common::setup_veth_pair();

    let monitor = start_route_monitor();

    let routed_destination = Ipv4Addr::new(203, 0, 113, 7);
    assert!(matches!(
        monitor.router.load().route_v4(routed_destination),
        Err(RouteError::NoRouteFound(_))
    ));

    common::replace_neighbor(links.right_ip, links.right_mac, &links.left_name);
    common::add_route("203.0.113.0/24", links.right_ip, &links.left_name);

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
    let _netns = common::NetNsGuard::new();
    let links = common::setup_veth_pair();

    let monitor = start_route_monitor();
    let routed_destination = Ipv4Addr::new(203, 0, 113, 7);

    common::add_route("203.0.113.0/24", links.right_ip, &links.left_name);
    let initial_mac = links.right_mac;
    common::replace_neighbor(links.right_ip, initial_mac, &links.left_name);

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
    common::replace_neighbor(links.right_ip, updated_mac, &links.left_name);

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

    common::delete_neighbor(links.right_ip, &links.left_name);
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
    let netns = common::NetNsGuard::new();
    let links = common::setup_veth_pair();

    common::replace_neighbor(links.right_ip, links.right_mac, &links.left_name);
    common::add_route("203.0.113.0/24", links.right_ip, &links.left_name);

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

    netns.ip(&["link", "del", &links.left_name]);
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
    let netns = common::NetNsGuard::new();
    let links = common::setup_veth_pair();

    let monitor = start_route_monitor();
    let overlay_destination = Ipv4Addr::new(192, 0, 2, 99);
    assert!(matches!(
        monitor.router.load().route_v4(overlay_destination),
        Err(RouteError::NoRouteFound(_))
    ));

    common::replace_neighbor(links.right_ip, links.right_mac, &links.left_name);
    common::add_route_to_dev(&format!("{}/32", links.right_ip), &links.left_name);
    let gre = common::setup_gre_tunnel(&links);
    common::add_route_to_dev_with_src("192.0.2.0/24", &gre.name, gre.overlay_ip);

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
                                && gre_route.mac_addr == links.right_mac
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

    netns.ip(&["link", "del", &gre.name]);
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
