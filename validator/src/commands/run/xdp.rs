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
        validator::{XdpModules, XdpTransmitSetup},
    },
    solana_net_utils::multihomed_sockets::BindIpAddrs,
    solana_poh::poh_service,
    std::{
        net::IpAddr,
        sync::{Arc, atomic::AtomicBool},
    },
};

pub(super) fn build_xdp_transmit_setup(
    mut xdp_config: XdpConfig,
    bind_addresses: &BindIpAddrs,
    exit: Arc<AtomicBool>,
) -> (XdpTransmitSetup, XdpNetworkConfigReport) {
    let device = if let Some(interface) = xdp_config.interface.as_ref() {
        NetworkDevice::new(interface).expect("configured interface should exist")
    } else {
        NetworkDevice::new_from_default_route().expect("default route device should exist")
    };

    let xdp_interface = device.name().to_string();
    // Keep the transmitter and metrics on the selected XDP device. Source IP lookup
    // uses the same interface name, with bond-master fallback.
    xdp_config.interface = Some(xdp_interface.clone());
    let zero_copy = xdp_config.zero_copy;
    let src_ip = match bind_addresses.active() {
        IpAddr::V4(ip) if !ip.is_unspecified() => ip,
        IpAddr::V4(_unspecified) => interface_ipv4(&xdp_interface)
            .expect("selected interface should exist and have an IPv4 address assigned"),
        _ => panic!("IPv6 not supported"),
    };
    // Nothing can express per-module queue assignments yet, so every
    // module transmits over the whole queue set.
    let all_positions: Box<[usize]> = (0..xdp_config.queues.len()).collect();
    (
        XdpTransmitSetup {
            transmitter_builder: TransmitterBuilder::new(xdp_config, exit)
                .expect("failed to create xdp transmitter"),
            src_ip,
            modules: XdpModules {
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
    )
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
        std::net::{IpAddr, Ipv4Addr},
    };

    fn single_ip_bind() -> BindIpAddrs {
        BindIpAddrs::new(vec![Ipv4Addr::UNSPECIFIED.into()]).unwrap()
    }

    fn multihoming_bind() -> BindIpAddrs {
        BindIpAddrs::new(vec![
            IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
            IpAddr::V4(Ipv4Addr::new(2, 2, 2, 2)),
        ])
        .unwrap()
    }

    #[test]
    fn test_no_xdp_flag_disables_xdp() {
        let default_args = DefaultArgs::default();
        let app = add_args(clap::App::new("agave-validator"), &default_args);
        let matches = app.get_matches_from(vec!["agave-validator", "--no-xdp"]);
        let result = build_xdp_config(&matches, &Operation::Run, &single_ip_bind());
        assert!(result.unwrap().is_none(), "--no-xdp must disable XDP");
    }

    #[test]
    fn test_empty_xdp_cpu_cores_is_error() {
        let default_args = DefaultArgs::default();
        let app = add_args(clap::App::new("agave-validator"), &default_args);
        let matches = app.get_matches_from(vec!["agave-validator", "--xdp-cpu-cores", "5-3"]);
        let result = build_xdp_config(&matches, &Operation::Run, &single_ip_bind());
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
        let result = build_xdp_config(&matches, &Operation::Initialize, &single_ip_bind());
        assert!(result.unwrap().is_none(), "init operation must disable XDP");
    }

    #[test]
    fn test_multihoming_is_error() {
        let default_args = DefaultArgs::default();
        let app = add_args(clap::App::new("agave-validator"), &default_args);
        let matches = app.get_matches_from(vec!["agave-validator"]);
        let result = build_xdp_config(&matches, &Operation::Run, &multihoming_bind());
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
        let result = build_xdp_config(&matches, &Operation::Run, &single_ip_bind());
        assert!(
            result.unwrap_err().contains("PoH core"),
            "XDP core overlapping PoH core must produce an error"
        );
    }
}
