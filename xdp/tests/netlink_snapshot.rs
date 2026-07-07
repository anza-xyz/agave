#![cfg(target_os = "linux")]

mod common;

use {
    agave_xdp::{
        netlink::{netlink_get_interfaces, netlink_get_neighbors, netlink_get_routes},
        route::RouteTable,
    },
    libc::{AF_INET, NUD_PERMANENT},
    std::net::{IpAddr, Ipv4Addr},
};

#[test]
#[ignore = "requires root and network namespace privileges"]
fn netlink_snapshot_reads_the_prepared_namespace() {
    let _netns = common::NetNsGuard::new().expect("create network namespace");
    let links = common::setup_veth_pair();

    let routed_prefix = "203.0.113.0/24";
    common::replace_neighbor(links.right_ip, links.right_mac, common::LEFT_IFACE);
    common::add_route(routed_prefix, links.right_ip, common::LEFT_IFACE);

    let interfaces = netlink_get_interfaces(AF_INET as u8).expect("read interfaces from netlink");
    assert!(
        interfaces
            .iter()
            .any(|interface| interface.if_index == links.left_if_index)
    );
    assert!(
        interfaces
            .iter()
            .any(|interface| interface.if_index == links.right_if_index)
    );

    let routes =
        netlink_get_routes(AF_INET as u8, u32::from(RouteTable::Main)).expect("read routes");
    assert!(routes.iter().any(|route| {
        route.destination == Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 0)))
            && route.gateway == Some(IpAddr::V4(links.right_ip))
            && route.out_if_index == Some(links.left_if_index as i32)
            && route.dst_len == 24
    }));

    let neighbors =
        netlink_get_neighbors(None, AF_INET as u8).expect("read neighbor table from netlink");
    assert!(neighbors.iter().any(|neighbor| {
        neighbor.destination == Some(IpAddr::V4(links.right_ip))
            && neighbor.lladdr == Some(links.right_mac)
            && neighbor.ifindex == links.left_if_index as i32
            && neighbor.state == NUD_PERMANENT
    }));
}

#[test]
#[ignore = "requires root and network namespace privileges"]
fn netlink_snapshot_reads_gre_tunnel_metadata() {
    let _netns = common::NetNsGuard::new().expect("create network namespace");
    let links = common::setup_veth_pair();
    let gre = common::setup_gre_tunnel(&links);

    let interfaces = netlink_get_interfaces(AF_INET as u8).expect("read interfaces from netlink");
    let tunnel = interfaces
        .iter()
        .find(|interface| interface.if_index == gre.if_index)
        .and_then(|interface| interface.gre_tunnel.as_ref())
        .expect("read GRE tunnel metadata from netlink");

    assert_eq!(tunnel.local, IpAddr::V4(gre.local_ip));
    assert_eq!(tunnel.remote, IpAddr::V4(gre.remote_ip));
    assert_eq!(tunnel.ttl, 64);
    assert_eq!(tunnel.tos, 0);
}
