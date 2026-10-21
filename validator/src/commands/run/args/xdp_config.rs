#[cfg(target_os = "linux")]
use {
    agave_cpu_utils::cpu_affinity,
    agave_xdp::transmitter::{QueueCpuBinding, XdpConfig},
    clap::ArgMatches,
    log::info,
    solana_clap_utils::input_parsers::{parse_cpu_ranges, value_of},
    solana_net_utils::multihomed_sockets::BindIpAddrs,
    solana_poh::poh_service,
};
use {clap::Arg, solana_clap_utils::input_validators::validate_cpu_ranges};

#[cfg(target_os = "linux")]
pub(crate) fn new_config(
    matches: &ArgMatches,
    bind_addresses: &BindIpAddrs,
) -> Result<Option<XdpConfig>, String> {
    if matches.is_present("no_xdp") {
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

pub(crate) fn args<'a, 'b>() -> Vec<Arg<'a, 'b>> {
    vec![
        Arg::with_name("no_xdp")
            .long("no-xdp")
            .takes_value(false)
            .conflicts_with("no_xdp_zero_copy")
            .help("Disable XDP transmit and fall back to UDP sockets"),
        Arg::with_name("xdp_interface")
            .long("xdp-interface")
            .takes_value(true)
            .value_name("INTERFACE")
            .conflicts_with("no_xdp")
            .help(
                "Network interface to use for XDP transmit. Auto-detected from default route if \
                 not specified",
            ),
        Arg::with_name("xdp_cpu_cores")
            .long("xdp-cpu-cores")
            .takes_value(true)
            .value_name("CPU_LIST")
            .conflicts_with("no_xdp")
            .validator(|value| validate_cpu_ranges(value, "--xdp-cpu-cores"))
            .help(
                "CPU cores to reserve for XDP transmit (e.g. \"2-4,7\"). Defaults to 1 \
                 auto-selected core",
            ),
        Arg::with_name("xdp_zero_copy")
            .long("xdp-zero-copy")
            .takes_value(false)
            .conflicts_with("no_xdp")
            .conflicts_with("no_xdp_zero_copy")
            .help("Enable XDP zero copy mode. Requires hardware and driver support"),
        Arg::with_name("no_xdp_zero_copy")
            .long("no-xdp-zero-copy")
            .takes_value(false)
            .conflicts_with("xdp_zero_copy")
            .conflicts_with("no_xdp")
            .help("Use XDP copy mode"),
    ]
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use {
        super::*,
        crate::{cli::DefaultArgs, commands::run::args::add_args},
        std::net::{IpAddr, Ipv4Addr},
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
    fn test_no_xdp_flag_disables_xdp() {
        let default_args = DefaultArgs::default();
        let app = add_args(clap::App::new("agave-validator"), &default_args);
        let matches = app.get_matches_from(vec!["agave-validator", "--no-xdp"]);
        let result = new_config(&matches, &build_single_ip_bind());
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
            let config = new_config(&matches, &build_single_ip_bind())
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
        let result = new_config(&matches, &build_single_ip_bind());
        assert!(
            result.unwrap_err().contains("selects no CPUs"),
            "empty XDP CPU core selection must produce an error"
        );
    }

    #[test]
    fn test_multihoming_is_error() {
        let default_args = DefaultArgs::default();
        let app = add_args(clap::App::new("agave-validator"), &default_args);
        let matches = app.get_matches_from(vec!["agave-validator"]);
        let result = new_config(&matches, &build_multihoming_bind());
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
        let result = new_config(&matches, &build_single_ip_bind());
        assert!(
            result.unwrap_err().contains("PoH core"),
            "XDP core overlapping PoH core must produce an error"
        );
    }
}
