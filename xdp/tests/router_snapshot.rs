#![cfg(target_os = "linux")]

mod common;

use {
    agave_xdp::route::{RouteTable, Router, RoutingTables},
    std::net::{IpAddr, Ipv4Addr},
};

#[test]
#[ignore = "requires root and network namespace privileges"]
fn router_snapshot_resolves_gre_routes_from_netlink() {
    let _netns = common::NetNsGuard::new().expect("create network namespace");
    let links = common::setup_veth_pair_with_tx_queue_count(1);

    common::replace_neighbor(links.right_ip, links.right_mac, common::LEFT_IFACE);
    common::add_route_to_dev(&format!("{}/32", links.right_ip), common::LEFT_IFACE);
    let gre = common::setup_gre_tunnel(&links);
    common::add_route_to_dev_with_src("192.0.2.0/24", common::GRE_IFACE, gre.overlay_ip);

    let router_from_tables =
        Router::from_tables(RoutingTables::from_netlink(RouteTable::Main).expect("read tables"))
            .expect("build router from snapshot tables");
    let router_from_netlink = Router::new().expect("build router directly from netlink");
    let overlay_destination = Ipv4Addr::new(192, 0, 2, 99);

    for router in [&router_from_tables, &router_from_netlink] {
        let next_hop = router
            .route_v4(overlay_destination)
            .expect("resolve GRE overlay route");
        assert_eq!(next_hop.if_index, gre.if_index);
        assert_eq!(next_hop.ip_addr, IpAddr::V4(overlay_destination));
        assert_eq!(next_hop.mac_addr, Some(links.right_mac));
        assert_eq!(next_hop.preferred_src_ip, Some(gre.overlay_ip));

        let gre_route = next_hop.gre.as_ref().expect("route should use GRE");
        assert_eq!(gre_route.if_index, gre.if_index);
        assert_eq!(gre_route.mac_addr, links.right_mac);
        assert_eq!(gre_route.tunnel_info.local, IpAddr::V4(gre.local_ip));
        assert_eq!(gre_route.tunnel_info.remote, IpAddr::V4(gre.remote_ip));
        assert_eq!(gre_route.tunnel_info.ttl, 64);
        assert_eq!(gre_route.tunnel_info.tos, 0);
    }
}
