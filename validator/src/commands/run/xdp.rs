use {
    agave_xdp::{
        device::NetworkDevice,
        interface_ipv4,
        transmitter::{TransmitterBuilder, XdpConfig},
    },
    solana_core::{
        system_monitor_service::XdpNetworkConfigReport,
        validator::{XdpComponents, XdpTransmitSetup},
    },
    solana_net_utils::multihomed_sockets::BindIpAddrs,
    std::{
        net::{IpAddr, Ipv4Addr},
        sync::{Arc, atomic::AtomicBool},
    },
};

pub(super) fn build_xdp_transmit_setup(
    mut xdp_config: XdpConfig,
    bind_addresses: &BindIpAddrs,
    exit: Arc<AtomicBool>,
) -> Result<(XdpTransmitSetup, XdpNetworkConfigReport), String> {
    let device = if let Some(interface) = xdp_config.interface.as_ref() {
        NetworkDevice::new(interface)
            .map_err(|err| format!("failed to open XDP interface {interface:?}: {err}"))?
    } else {
        NetworkDevice::new_from_default_route().map_err(|err| {
            format!("failed to select XDP interface from the default route: {err}")
        })?
    };

    let xdp_interface = device.name().to_string();
    // Keep the transmitter and metrics on the selected XDP device. Source IP lookup
    // uses the same interface name, with bond-master fallback.
    xdp_config.interface = Some(xdp_interface.clone());
    let zero_copy = xdp_config.zero_copy;
    let src_ip = resolve_xdp_source_ip(bind_addresses.active(), &xdp_interface)?;
    let transmitter_builder = TransmitterBuilder::new(xdp_config, exit)
        .map_err(|err| format!("failed to create XDP transmitter on {xdp_interface}: {err}"))?;
    // Nothing can express per-component queue assignments yet, so every
    // component transmits over the whole queue set.
    let all_positions: Box<[usize]> = (0..transmitter_builder.sender_count()).collect();
    Ok((
        XdpTransmitSetup {
            transmitter_builder,
            src_ip,
            components: XdpComponents {
                tpu: Some(all_positions.clone()),
                turbine: Some(all_positions.clone()),
                repair: Some(all_positions.clone()),
                gossip: Some(all_positions.clone()),
                votor: Some(all_positions),
            },
        },
        XdpNetworkConfigReport {
            zero_copy,
            interface: xdp_interface,
        },
    ))
}

fn resolve_xdp_source_ip(bind_address: IpAddr, interface: &str) -> Result<Ipv4Addr, String> {
    match bind_address {
        IpAddr::V4(ip) if !ip.is_unspecified() => Ok(ip),
        IpAddr::V4(_) => interface_ipv4(interface)
            .map_err(|err| format!("failed to resolve XDP source IPv4 address: {err}")),
        IpAddr::V6(_) => Err("XDP does not support IPv6 bind addresses".to_string()),
    }
}

#[cfg(test)]
mod xdp_tests {
    use {
        super::*,
        agave_xdp::transmitter::QueueCpuBinding,
        solana_net_utils::multihomed_sockets::BindIpAddrs,
        std::net::{IpAddr, Ipv4Addr, Ipv6Addr},
    };

    #[test]
    fn test_resolve_xdp_source_ip() {
        let address = Ipv4Addr::new(192, 0, 2, 1);
        for (bind, interface, expected) in [
            (IpAddr::V4(address), "", Ok(address)),
            (
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                "lo",
                Ok(Ipv4Addr::LOCALHOST),
            ),
            (
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                "",
                Err("failed to resolve XDP source IPv4 address"),
            ),
            (
                IpAddr::V6(Ipv6Addr::LOCALHOST),
                "",
                Err("does not support IPv6"),
            ),
        ] {
            let result = resolve_xdp_source_ip(bind, interface);
            match expected {
                Ok(expected) => assert_eq!(
                    result.expect("a valid IPv4 source address should resolve"),
                    expected,
                    "source IPv4 selection must use the explicit bind address or fall back to the \
                     selected interface for unspecified binds: {bind} on {interface:?}"
                ),
                Err(expected) => {
                    let error = result.expect_err("invalid source address setup should fail");
                    assert!(
                        error.contains(expected),
                        "invalid XDP source address setup must return a descriptive error \
                         containing {expected:?}: {bind} on {interface:?}: {error}"
                    );
                }
            }
        }
    }

    #[test]
    fn test_build_xdp_transmit_setup_reports_transmitter_error() {
        let result = build_xdp_transmit_setup(
            XdpConfig::new(
                Some("lo"),
                vec![QueueCpuBinding {
                    queue: 0,
                    cpu: usize::MAX,
                }],
                false,
            ),
            &BindIpAddrs::new(vec![IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))])
                .expect("a single specified IPv4 bind address should be valid"),
            Arc::new(AtomicBool::new(false)),
        );
        let error = result
            .err()
            .expect("an invalid CPU ID should fail transmitter initialization");
        assert!(
            error.contains("failed to create XDP transmitter on lo: invalid CPU"),
            "transmitter errors must identify the selected interface and the failure: {error}"
        );
    }
}
