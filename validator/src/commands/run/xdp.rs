use {
    super::execute::Operation,
    agave_cpu_utils::cpu_affinity,
    agave_xdp::{
        device::NetworkDevice,
        interface_ipv4,
        transmitter::{QueueCpuBinding, TransmitterBuilder, XdpConfig},
    },
    clap::ArgMatches,
    log::info,
    solana_clap_utils::input_parsers::{parse_cpu_ranges, value_of},
    solana_core::{
        system_monitor_service::XdpNetworkConfigReport,
        validator::{XdpComponents, XdpTransmitSetup},
    },
    solana_net_utils::multihomed_sockets::BindIpAddrs,
    solana_poh::poh_service,
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

pub(super) fn build_xdp_config(
    matches: &ArgMatches,
    operation: &Operation,
    bind_addresses: &BindIpAddrs,
) -> Result<Option<XdpConfig>, String> {
    if matches.is_present("no_xdp") || *operation == Operation::Initialize {
        return Ok(None);
    }
    if bind_addresses.len() > 1 {
        return Err(
            "XDP cannot be used in a multihoming context; pass --no-xdp to disable XDP".to_string(),
        );
    }
    let xdp_interface = matches.value_of("xdp_interface");
    let xdp_zero_copy = matches.is_present("xdp_zero_copy");
    let poh_pinned_cpu_core = value_of(matches, "poh_pinned_cpu_core")
        .or_else(|| value_of(matches, "experimental_poh_pinned_cpu_core"))
        .or(poh_service::DEFAULT_PINNED_CPU_CORE);
    let xdp_cpu_cores = matches.value_of("xdp_cpu_cores");
    let cpus = if let Some(cpu_str) = xdp_cpu_cores {
        let parsed =
            parse_cpu_ranges(cpu_str).expect("clap validator already accepted this CPU list");
        if parsed.is_empty() {
            return Err(format!("--xdp-cpu-cores `{cpu_str}` selects no CPUs"));
        }
        if let Some(poh_core) = poh_pinned_cpu_core
            && parsed.contains(&poh_core)
        {
            return Err(format!(
                "--xdp-cpu-cores includes PoH core {poh_core}; XDP and PoH must not share a CPU \
                 core"
            ));
        }
        Some(parsed)
    } else {
        // Auto-select a single core, avoiding the PoH core.
        match cpu_affinity(None) {
            Ok(allowed) => {
                match allowed
                    .iter()
                    .rev()
                    .map(|cpu| **cpu)
                    .find(|cpu| Some(*cpu) != poh_pinned_cpu_core)
                {
                    Some(cpu) => Some(vec![cpu]),
                    None => {
                        return Err(format!(
                            "XDP requires a dedicated CPU core separate from PoH (core \
                             {poh_pinned_cpu_core:?}), but none is available. Pass --no-xdp to \
                             disable XDP."
                        ));
                    }
                }
            }
            Err(e) => {
                return Err(format!(
                    "failed to query CPU affinity: {e}. Pass --no-xdp to disable XDP, or provide \
                     --xdp-cpu-cores explicitly."
                ));
            }
        }
    };
    Ok(cpus.map(|cpus| {
        info!("XDP enabled on CPU cores: {cpus:?}");
        // Map the CPU list onto hardware queues sequentially (queue i -> cpus[i]).
        let queues = cpus
            .into_iter()
            .enumerate()
            .map(|(queue, cpu)| QueueCpuBinding {
                queue: queue as u32,
                cpu,
            })
            .collect();
        XdpConfig::new(xdp_interface, queues, xdp_zero_copy)
    }))
}

#[cfg(test)]
mod xdp_tests {
    use {
        super::*,
        crate::{cli::DefaultArgs, commands::run::args::add_args},
        solana_net_utils::multihomed_sockets::BindIpAddrs,
        std::net::{IpAddr, Ipv4Addr, Ipv6Addr},
    };

    fn build_single_ip_bind() -> BindIpAddrs {
        BindIpAddrs::new(vec![Ipv4Addr::UNSPECIFIED.into()])
            .expect("a single unspecified IPv4 bind address should be valid")
    }

    fn build_multihoming_bind() -> BindIpAddrs {
        BindIpAddrs::new(vec![
            IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
            IpAddr::V4(Ipv4Addr::new(2, 2, 2, 2)),
        ])
        .expect("two distinct specified IPv4 bind addresses should be valid")
    }

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

    #[test]
    fn test_no_xdp_flag_disables_xdp() {
        let default_args = DefaultArgs::default();
        let app = add_args(clap::App::new("agave-validator"), &default_args);
        let matches = app.get_matches_from(vec!["agave-validator", "--no-xdp"]);
        let result = build_xdp_config(&matches, &Operation::Run, &build_single_ip_bind());
        assert!(
            result
                .expect("--no-xdp should bypass XDP configuration validation")
                .is_none(),
            "--no-xdp must disable XDP"
        );
    }

    #[test]
    fn test_xdp_copy_mode_selection() {
        for (flag, zero_copy) in [
            (None, false),
            (Some("--xdp-zero-copy"), true),
            (Some("--no-xdp-zero-copy"), false),
        ] {
            let default_args = DefaultArgs::default();
            let app = add_args(clap::App::new("agave-validator"), &default_args);
            let mut args = vec![
                "agave-validator",
                "--xdp-cpu-cores",
                "1",
                "--poh-pinned-cpu-core",
                "0",
            ];
            args.extend(flag);
            let matches = app
                .get_matches_from_safe(args)
                .expect("valid XDP copy mode selection should be accepted");
            let config = build_xdp_config(&matches, &Operation::Run, &build_single_ip_bind())
                .expect("distinct XDP and PoH cores should be valid")
                .expect("copy mode selection should keep XDP enabled");
            assert_eq!(
                config.zero_copy, zero_copy,
                "XDP copy mode must match the selection {flag:?}"
            );
        }
    }

    #[test]
    fn test_empty_xdp_cpu_cores_is_error() {
        let default_args = DefaultArgs::default();
        let app = add_args(clap::App::new("agave-validator"), &default_args);
        let matches = app.get_matches_from(vec!["agave-validator", "--xdp-cpu-cores", "5-3"]);
        let result = build_xdp_config(&matches, &Operation::Run, &build_single_ip_bind());
        assert!(
            result.unwrap_err().contains("selects no CPUs"),
            "empty XDP CPU core selection must produce an error"
        );
    }

    #[test]
    fn test_init_disables_xdp() {
        let default_args = DefaultArgs::default();
        let app = add_args(clap::App::new("agave-validator"), &default_args);
        let matches = app.get_matches_from(vec!["agave-validator"]);
        let result = build_xdp_config(&matches, &Operation::Initialize, &build_single_ip_bind());
        assert!(
            result
                .expect("initialization should bypass XDP configuration validation")
                .is_none(),
            "init operation must disable XDP"
        );
    }

    #[test]
    fn test_multihoming_is_error() {
        let default_args = DefaultArgs::default();
        let app = add_args(clap::App::new("agave-validator"), &default_args);
        let matches = app.get_matches_from(vec!["agave-validator"]);
        let result = build_xdp_config(&matches, &Operation::Run, &build_multihoming_bind());
        assert!(
            result.unwrap_err().contains("multihoming"),
            "multihoming context must produce an error"
        );
    }

    #[test]
    fn test_explicit_xdp_core_conflicts_with_poh_core_is_error() {
        let default_args = DefaultArgs::default();
        let app = add_args(clap::App::new("agave-validator"), &default_args);
        let poh_core = solana_poh::poh_service::DEFAULT_PINNED_CPU_CORE
            .unwrap_or(0)
            .to_string();
        let matches = app.get_matches_from(vec!["agave-validator", "--xdp-cpu-cores", &poh_core]);
        let result = build_xdp_config(&matches, &Operation::Run, &build_single_ip_bind());
        assert!(
            result.unwrap_err().contains("PoH core"),
            "XDP core overlapping PoH core must produce an error"
        );
    }
}
