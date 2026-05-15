#![cfg(target_os = "linux")]

mod common;

use {
    agave_xdp::route::{RouteTable, Router, RoutingTables},
    std::net::{IpAddr, Ipv4Addr},
};

#[test]
fn router_snapshot_rebuilds_main_table_routes_from_netlink() {
    let netns = common::NetNsGuard::new();
    let primary = common::setup_veth_pair();
    let backup = common::setup_backup_veth_pair();

    common::replace_neighbor(primary.right_ip, primary.right_mac, &primary.left_name);
    common::replace_neighbor(backup.right_ip, backup.right_mac, &backup.left_name);

    let primary_gateway = primary.right_ip.to_string();
    let backup_gateway = backup.right_ip.to_string();
    let backup_src = backup.left_ip.to_string();

    netns.ip(&[
        "route",
        "replace",
        "default",
        "via",
        &backup_gateway,
        "dev",
        &backup.left_name,
        "metric",
        "200",
    ]);
    netns.ip(&[
        "route",
        "replace",
        "default",
        "via",
        &primary_gateway,
        "dev",
        &primary.left_name,
        "metric",
        "100",
    ]);
    netns.ip(&[
        "route",
        "replace",
        "203.0.113.0/24",
        "via",
        &backup_gateway,
        "dev",
        &backup.left_name,
        "src",
        &backup_src,
        "metric",
        "50",
    ]);
    netns.ip(&[
        "route",
        "replace",
        "198.51.100.0/24",
        "via",
        &backup_gateway,
        "dev",
        &backup.left_name,
        "table",
        "100",
    ]);

    let router_from_tables =
        Router::from_tables(RoutingTables::from_netlink(RouteTable::Main).expect("read tables"))
            .expect("build router from snapshot tables");
    let router_from_netlink = Router::new().expect("build router directly from netlink");

    for router in [&router_from_tables, &router_from_netlink] {
        let default_route = router.default().expect("resolve default route");
        assert_eq!(default_route.if_index, primary.left_if_index);
        assert_eq!(default_route.ip_addr, IpAddr::V4(primary.right_ip));
        assert_eq!(default_route.mac_addr, Some(primary.right_mac));
        assert!(default_route.gre.is_none());

        let specific_route = router
            .route_v4(Ipv4Addr::new(203, 0, 113, 7))
            .expect("resolve main-table specific route");
        assert_eq!(specific_route.if_index, backup.left_if_index);
        assert_eq!(specific_route.ip_addr, IpAddr::V4(backup.right_ip));
        assert_eq!(specific_route.mac_addr, Some(backup.right_mac));
        assert_eq!(specific_route.preferred_src_ip, Some(backup.left_ip));
        assert!(specific_route.gre.is_none());

        let route_only_present_in_other_table = router
            .route_v4(Ipv4Addr::new(198, 51, 100, 7))
            .expect("fall back to the main-table default route");
        assert_eq!(
            route_only_present_in_other_table.if_index,
            primary.left_if_index
        );
        assert_eq!(
            route_only_present_in_other_table.ip_addr,
            IpAddr::V4(primary.right_ip)
        );
        assert_eq!(
            route_only_present_in_other_table.mac_addr,
            Some(primary.right_mac)
        );
    }
}

#[test]
fn router_snapshot_resolves_gre_routes_from_netlink() {
    let _netns = common::NetNsGuard::new();
    let links = common::setup_veth_pair();

    common::replace_neighbor(links.right_ip, links.right_mac, &links.left_name);
    common::add_route_to_dev(&format!("{}/32", links.right_ip), &links.left_name);
    let gre = common::setup_gre_tunnel(&links);
    common::add_route_to_dev_with_src("192.0.2.0/24", &gre.name, gre.overlay_ip);

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
