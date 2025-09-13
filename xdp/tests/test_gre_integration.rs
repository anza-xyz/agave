#![allow(clippy::arithmetic_side_effects)]
use {
    agave_xdp::{
        netlink::InterfaceInfo,
        packet::{
            construct_gre_packet, write_eth_header, write_ip_header_for_udp, write_udp_header,
            ETH_HEADER_SIZE, GRE_HEADER_SIZE, IP_HEADER_SIZE, UDP_HEADER_SIZE,
        },
        route::{AtomicRouter, NextHop, Router},
    },
    libc::ARPHRD_IPGRE,
    std::net::{IpAddr, Ipv4Addr},
};

fn setup_mock_router() -> Result<AtomicRouter, Box<dyn std::error::Error>> {
    let mock_router = Router::new_with_mock_data();
    let atomic_router = AtomicRouter::new()?;
    atomic_router.set_mock_router(mock_router);
    Ok(atomic_router)
}

#[test]
fn test_regular_interface_packet_construction() -> Result<(), Box<dyn std::error::Error>> {
    // Setup mock router
    let atomic_router = setup_mock_router()?;

    let dst_ip = Ipv4Addr::new(10, 0, 0, 1); // Regular interface (eth0)
    let src_ip = Ipv4Addr::new(192, 168, 100, 1);
    let src_port = 12345;
    let payload = b"Test payload for regular interface";

    // Get route and interface info
    let (next_hop, interface_info) = atomic_router.load().route(IpAddr::V4(dst_ip))?;

    // Should not require GRE wrapping
    let requires_gre = interface_info.dev_type == ARPHRD_IPGRE;
    assert!(
        !requires_gre,
        "Regular interface should not require GRE wrapping"
    );

    // Test regular packet construction
    if next_hop.mac_addr.is_some() {
        test_regular_packet_construction(dst_ip, src_ip, src_port, payload, &next_hop)?;
    } else {
        panic!("Regular interface should have MAC address");
    }

    // test another regular interface
    let dst_ip = Ipv4Addr::new(10, 0, 0, 3); // Another regular interface (eth1)
    let src_ip = Ipv4Addr::new(192, 168, 100, 1);
    let src_port = 12345;
    let payload = b"Test payload for another regular interface";

    // Get route and interface info
    let (next_hop, interface_info) = atomic_router.load().route(IpAddr::V4(dst_ip))?;

    // Should not require GRE wrapping
    let requires_gre = interface_info.dev_type == ARPHRD_IPGRE;
    assert!(
        !requires_gre,
        "Another regular interface should not require GRE wrapping"
    );

    // Test regular packet construction
    if next_hop.mac_addr.is_some() {
        test_regular_packet_construction(dst_ip, src_ip, src_port, payload, &next_hop)?;
    } else {
        panic!("Regular interface should have MAC address");
    }

    Ok(())
}

#[test]
fn test_gre_interface_packet_construction() -> Result<(), Box<dyn std::error::Error>> {
    // Setup mock router
    let atomic_router = setup_mock_router()?;

    let dst_ip = Ipv4Addr::new(10, 0, 0, 2); // GRE interface (gre0)
    let src_ip = Ipv4Addr::new(192, 168, 100, 1);
    let src_port = 12345;
    let payload = b"Test payload for GRE interface";

    // Get route and interface info
    let (_next_hop, interface_info) = atomic_router.load().route(IpAddr::V4(dst_ip))?;

    // Should require GRE wrapping
    let requires_gre = interface_info.dev_type == ARPHRD_IPGRE;
    assert!(requires_gre, "GRE interface should require GRE wrapping");

    // Test GRE packet construction
    if interface_info.gre_tunnel.is_some() {
        test_gre_packet_construction(
            &atomic_router,
            dst_ip,
            src_ip,
            src_port,
            payload,
            &interface_info,
        )?;
    } else {
        panic!("GRE interface should have tunnel configuration");
    }

    Ok(())
}

fn test_gre_packet_construction(
    router: &AtomicRouter,
    dst_ip: Ipv4Addr,
    src_ip: Ipv4Addr,
    src_port: u16,
    payload: &[u8],
    interface_info: &InterfaceInfo,
) -> Result<(), Box<dyn std::error::Error>> {
    // Get GRE tunnel info
    let gre_tunnel = interface_info
        .gre_tunnel
        .as_ref()
        .ok_or("GRE interface missing tunnel configuration")?;

    // Get next hop MAC
    let next_hop = router.load().route(IpAddr::V4(dst_ip))?.0;
    let dst_mac = next_hop.mac_addr.ok_or("Missing MAC address")?;
    let src_mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];

    // Calculate packet sizes
    let inner_packet_len = IP_HEADER_SIZE + UDP_HEADER_SIZE + payload.len();
    let gre_packet_size = ETH_HEADER_SIZE + IP_HEADER_SIZE + GRE_HEADER_SIZE + inner_packet_len;

    // Create packet buffer
    let mut packet = vec![0u8; gre_packet_size];

    // Construct the GRE packet using the new function
    construct_gre_packet(
        &mut packet,
        &src_ip,
        &dst_ip,
        src_port,
        53, // DNS port
        payload,
        gre_tunnel.src_ip,
        gre_tunnel.dst_ip,
        &src_mac,
        &dst_mac.0,
    );

    // Validate GRE packet structure
    validate_gre_packet(
        &packet,
        gre_tunnel.src_ip,
        gre_tunnel.dst_ip,
        src_ip,
        dst_ip,
        payload,
    )?;
    Ok(())
}

fn test_regular_packet_construction(
    dst_ip: Ipv4Addr,
    src_ip: Ipv4Addr,
    src_port: u16,
    payload: &[u8],
    next_hop: &NextHop,
) -> Result<(), Box<dyn std::error::Error>> {
    let dst_mac = next_hop.mac_addr.ok_or("Missing MAC address")?;
    let src_mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];

    // Calculate packet size
    let packet_size = ETH_HEADER_SIZE + IP_HEADER_SIZE + UDP_HEADER_SIZE + payload.len();

    // Create packet buffer
    let mut packet = vec![0u8; packet_size];

    // Write Ethernet header
    write_eth_header(&mut packet[..ETH_HEADER_SIZE], &src_mac, &dst_mac.0);

    // Write IP header
    write_ip_header_for_udp(
        &mut packet[ETH_HEADER_SIZE..ETH_HEADER_SIZE + IP_HEADER_SIZE],
        &src_ip,
        &dst_ip,
        (UDP_HEADER_SIZE + payload.len()) as u16,
    );

    // Write UDP header
    write_udp_header(
        &mut packet
            [ETH_HEADER_SIZE + IP_HEADER_SIZE..ETH_HEADER_SIZE + IP_HEADER_SIZE + UDP_HEADER_SIZE],
        &src_ip,
        src_port,
        &dst_ip,
        53, // DNS port
        payload.len() as u16,
        false, // csum
    );

    // Write payload
    packet[ETH_HEADER_SIZE + IP_HEADER_SIZE + UDP_HEADER_SIZE..].copy_from_slice(payload);

    Ok(())
}

fn validate_gre_packet(
    packet: &[u8],
    gre_src_ip: Ipv4Addr,
    gre_dst_ip: Ipv4Addr,
    inner_src_ip: Ipv4Addr,
    inner_dst_ip: Ipv4Addr,
    expected_payload: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    // Check Ethernet header
    let eth_type = u16::from_be_bytes([packet[12], packet[13]]);
    assert_eq!(eth_type, 0x0800, "Ethernet type should be IP");

    // Check outer IP header
    let outer_ip_protocol = packet[ETH_HEADER_SIZE + 9];
    assert_eq!(outer_ip_protocol, 47, "Outer IP should be GRE protocol");

    let outer_src_ip = Ipv4Addr::new(
        packet[ETH_HEADER_SIZE + 12],
        packet[ETH_HEADER_SIZE + 13],
        packet[ETH_HEADER_SIZE + 14],
        packet[ETH_HEADER_SIZE + 15],
    );
    let outer_dst_ip = Ipv4Addr::new(
        packet[ETH_HEADER_SIZE + 16],
        packet[ETH_HEADER_SIZE + 17],
        packet[ETH_HEADER_SIZE + 18],
        packet[ETH_HEADER_SIZE + 19],
    );

    assert_eq!(
        outer_src_ip, gre_src_ip,
        "Outer IP source should match GRE src"
    );
    assert_eq!(
        outer_dst_ip, gre_dst_ip,
        "Outer IP destination should match GRE dst"
    );

    // Check GRE header
    let gre_start = ETH_HEADER_SIZE + IP_HEADER_SIZE;
    let gre_flags_version = u16::from_be_bytes([packet[gre_start], packet[gre_start + 1]]);
    let gre_protocol = u16::from_be_bytes([packet[gre_start + 2], packet[gre_start + 3]]);

    assert_eq!(gre_flags_version, 0x0000, "GRE should be basic (no flags)");
    assert_eq!(gre_protocol, 0x0800, "GRE should encapsulate IP");

    // Check inner IP header
    let inner_ip_start = ETH_HEADER_SIZE + IP_HEADER_SIZE + GRE_HEADER_SIZE;
    let inner_ip_protocol = packet[inner_ip_start + 9];
    assert_eq!(inner_ip_protocol, 17, "Inner IP should be UDP protocol");

    let inner_src_ip_actual = Ipv4Addr::new(
        packet[inner_ip_start + 12],
        packet[inner_ip_start + 13],
        packet[inner_ip_start + 14],
        packet[inner_ip_start + 15],
    );
    let inner_dst_ip_actual = Ipv4Addr::new(
        packet[inner_ip_start + 16],
        packet[inner_ip_start + 17],
        packet[inner_ip_start + 18],
        packet[inner_ip_start + 19],
    );

    assert_eq!(
        inner_src_ip_actual, inner_src_ip,
        "Inner IP source should match"
    );
    assert_eq!(
        inner_dst_ip_actual, inner_dst_ip,
        "Inner IP destination should match"
    );

    // Check payload
    let payload_start = inner_ip_start + IP_HEADER_SIZE + UDP_HEADER_SIZE;
    let actual_payload = &packet[payload_start..payload_start + expected_payload.len()];
    assert_eq!(
        actual_payload, expected_payload,
        "Payload should be preserved"
    );
    Ok(())
}

#[test]
fn test_route_lookup() -> Result<(), Box<dyn std::error::Error>> {
    let router = setup_mock_router()?.load();

    // Test route lookups with mock destinations
    let test_destinations = [
        Ipv4Addr::new(10, 0, 0, 1), // eth0
        Ipv4Addr::new(10, 0, 0, 2), // gre0
        Ipv4Addr::new(10, 0, 0, 3), // eth1
    ];

    for (i, dst_ip) in test_destinations.iter().enumerate() {
        let (next_hop, if_info) = router.route(IpAddr::V4(*dst_ip))?;
        assert_eq!(
            if_info.if_index,
            (i + 1) as u32,
            "Route {} should have interface index {}",
            i + 1,
            i + 1
        );

        match i {
            0 => assert_eq!(if_info.if_name, "eth0", "First route should be via eth0"),
            1 => assert_eq!(if_info.if_name, "gre0", "Second route should be via gre0"),
            2 => assert_eq!(if_info.if_name, "eth1", "Third route should be via eth1"),
            _ => panic!("Unexpected route index: {i}"),
        }

        // Test GRE interface detection
        let is_gre = router.is_gre_interface(if_info.if_index);
        match i {
            1 => assert!(is_gre, "gre0 should be detected as GRE interface"),
            _ => assert!(!is_gre, "Non-GRE interfaces should not be detected as GRE"),
        }

        assert!(
            next_hop.mac_addr.is_some(),
            "Route to {dst_ip} should have MAC address available",
        );

        // Test interface retrieval
        if let Some(interface) = router.get_interface(if_info.if_index) {
            assert_eq!(
                interface.if_name, if_info.if_name,
                "Retrieved interface name should match route interface name"
            );
            assert_eq!(
                interface.dev_type, if_info.dev_type,
                "Retrieved interface type should match route interface type"
            );
        } else {
            panic!(
                "Failed to retrieve interface details for {}",
                if_info.if_name
            );
        }
    }
    Ok(())
}
