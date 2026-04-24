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
