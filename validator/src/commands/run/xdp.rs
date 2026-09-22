//! Validator XDP policy loading, host resolution, and transmit setup.

use {
    super::execute::Operation,
    agave_validator_config as config,
    clap::ArgMatches,
    log::{info, warn},
    solana_clap_utils::input_parsers::parse_cpu_ranges,
    std::path::Path,
};
#[cfg(target_os = "linux")]
use {
    agave_cpu_utils::cpu_affinity,
    agave_xdp::device::NetworkDevice,
    solana_clap_utils::input_parsers::value_of,
    solana_core::{
        system_monitor_service::XdpNetworkConfigReport,
        validator::{XdpModules, XdpTransmitSetup},
    },
    solana_net_utils::multihomed_sockets::BindIpAddrs,
    solana_poh::poh_service,
    std::{
        collections::BTreeSet,
        net::{IpAddr, Ipv4Addr},
        sync::{Arc, atomic::AtomicBool},
    },
};

#[cfg(target_os = "linux")]
pub(super) struct ResolvedXdp {
    policy: config::RuntimeXdpConfig,
    device: NetworkDevice,
    src_ip: Ipv4Addr,
}

#[cfg(target_os = "linux")]
impl ResolvedXdp {
    pub(super) fn zero_copy(&self) -> bool {
        self.policy.zero_copy
    }
}

#[cfg(target_os = "linux")]
fn bind_address_conflict(count: usize, address: IpAddr) -> Option<&'static str> {
    if count > 1 {
        Some("XDP does not support multiple --bind-address values; select one IPv4 address")
    } else if address.is_ipv6() {
        Some("XDP transmit supports IPv4 only; supply an IPv4 --bind-address")
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
fn resolve_xdp_configuration(
    config: &config::EffectiveConfig,
    bind_address_count: usize,
    bind_address: IpAddr,
    poh_core: Option<usize>,
) -> Result<(Option<ResolvedXdp>, Vec<String>), String> {
    if !config.xdp_active() {
        return Ok((None, config::validate_policy(config)?));
    }
    if let Some(conflict) = bind_address_conflict(bind_address_count, bind_address) {
        return Err(format!("{conflict}, or pass --no-xdp"));
    }
    let allowed_cpus: BTreeSet<_> = cpu_affinity(None)
        .map_err(|error| format!("failed to query process CPU affinity for XDP: {error}"))?
        .into_iter()
        .map(|cpu| *cpu)
        .collect();
    let (policy, warnings) = config::resolve_runtime(config, &allowed_cpus, poh_core)?;
    let device = resolve_xdp_device(&policy.interface_label, &policy.device)?;
    let src_ip = resolve_xdp_source_ipv4(&policy.interface_label, &device, bind_address)?;
    Ok((
        Some(ResolvedXdp {
            policy,
            device,
            src_ip,
        }),
        warnings,
    ))
}

#[cfg(target_os = "linux")]
pub(super) fn build_xdp_transmit_setup(
    resolved: ResolvedXdp,
    exit: Arc<AtomicBool>,
) -> Result<(XdpTransmitSetup, XdpNetworkConfigReport), String> {
    use agave_xdp::transmitter::{QueueCpuBinding, TransmitterBuilder, XdpConfig};

    let ResolvedXdp {
        policy,
        device,
        src_ip,
    } = resolved;
    let config::RuntimeXdpConfig {
        interface_label: logical_interface,
        device: _,
        queues,
        zero_copy,
        components,
    } = policy;
    let modules = XdpModules {
        tpu: Some(components.tpu),
        turbine: Some(components.turbine),
        repair: Some(components.repair),
        gossip: Some(components.gossip),
        votor: Some(components.votor),
    };
    let xdp_interface = device.name().to_string();
    let queues = queues
        .into_iter()
        .map(|binding| QueueCpuBinding {
            queue: binding.queue,
            cpu: binding.cpu,
        })
        .collect();
    let transmitter_builder = TransmitterBuilder::new(
        XdpConfig::new(Some(xdp_interface.clone()), queues, zero_copy),
        exit,
    )
    .map_err(|e| {
        let remediation = if zero_copy {
            "Check the configured workers; if zero-copy is unsupported, pass --no-xdp-zero-copy, \
             or pass --no-xdp."
        } else {
            "Check the configured workers or pass --no-xdp."
        };
        format!(
            "failed to create the XDP transmitter for logical interface `{logical_interface}`, \
             device `{xdp_interface}`: {e}. {remediation}"
        )
    })?;
    Ok((
        XdpTransmitSetup {
            transmitter_builder,
            src_ip,
            modules,
        },
        XdpNetworkConfigReport {
            zero_copy,
            interface: xdp_interface,
        },
    ))
}

#[cfg(target_os = "linux")]
fn resolve_xdp_device(
    logical_interface: &str,
    selector: &config::DeviceSelector,
) -> Result<NetworkDevice, String> {
    match selector {
        config::DeviceSelector::Name(name) => NetworkDevice::new(name).map_err(|error| {
            format!(
                "XDP logical interface `{logical_interface}` selects device.name {name:?}, which \
                 is not usable: {error}; fix the name or pass --no-xdp"
            )
        }),
        config::DeviceSelector::Route(config::RouteSelector::Default) => {
            NetworkDevice::new_from_default_route().map_err(|error| {
                format!(
                    "failed to open the default-route device for XDP logical interface \
                     `{logical_interface}`: {error}; set device.name or pass --no-xdp"
                )
            })
        }
    }
}

#[cfg(target_os = "linux")]
fn resolve_xdp_source_ipv4(
    logical_interface: &str,
    device: &NetworkDevice,
    bind_ip: IpAddr,
) -> Result<Ipv4Addr, String> {
    match bind_ip {
        IpAddr::V4(ip) if !ip.is_unspecified() => Ok(ip),
        IpAddr::V4(_) => agave_xdp::interface_ipv4(device.name()).map_err(|error| {
            format!(
                "cannot select an IPv4 source address for XDP logical interface \
                 `{logical_interface}`, device `{}`: {error}; assign an IPv4 address to the \
                 device, pass --bind-address, or pass --no-xdp",
                device.name()
            )
        }),
        IpAddr::V6(_) => Err(
            "XDP transmit supports IPv4 only; supply an IPv4 --bind-address or pass --no-xdp"
                .to_string(),
        ),
    }
}

fn load_xdp_policy(
    matches: &ArgMatches,
    operation: &Operation,
) -> Result<Option<config::EffectiveConfig>, String> {
    let effective = config::load(matches.value_of("experimental_config_file").map(Path::new))?;
    let overrides = cli_xdp_overrides(matches)?;
    if *operation == Operation::Initialize {
        info!("ledger initialization does not start XDP; skipping XDP policy validation");
        return Ok(None);
    }
    let application = config::apply_cli(effective, overrides)?;
    for warning in application.warnings {
        warn!("{warning}");
    }
    Ok(Some(application.config))
}

#[cfg(any(not(target_os = "linux"), test))]
pub(super) fn validate_config_file_without_xdp(
    matches: &ArgMatches,
    operation: &Operation,
) -> Result<(), String> {
    let Some(config) = load_xdp_policy(matches, operation)? else {
        return Ok(());
    };
    for warning in config::validate_policy(&config)? {
        warn!("{warning}");
    }
    // Only report inactivity the operator can act on. The built-in policy enables
    // XDP everywhere, so warning about it unprompted would fire on every startup.
    if matches.is_present("experimental_config_file") && config.xdp_active() {
        warn!(
            "XDP transmit is unavailable on this platform; the configured XDP policy is valid but \
             inactive"
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn build_xdp_config(
    matches: &ArgMatches,
    operation: &Operation,
    bind_addresses: &BindIpAddrs,
) -> Result<Option<ResolvedXdp>, String> {
    let Some(config) = load_xdp_policy(matches, operation)? else {
        return Ok(None);
    };
    let poh_pinned_cpu_core = value_of(matches, "poh_pinned_cpu_core")
        .or_else(|| value_of(matches, "experimental_poh_pinned_cpu_core"))
        .or(poh_service::DEFAULT_PINNED_CPU_CORE);
    let (resolved, warnings) = resolve_xdp_configuration(
        &config,
        bind_addresses.len(),
        bind_addresses.active(),
        poh_pinned_cpu_core,
    )?;
    for warning in warnings {
        warn!("{warning}");
    }
    if let Some(runtime) = &resolved {
        info!(
            "XDP policy: label={}, selector={:?}, device={}, source_ipv4={}, zero_copy={}, \
             workers={:?}, component sender positions: tpu={:?}, turbine={:?}, repair={:?}, \
             gossip={:?}, votor={:?}",
            runtime.policy.interface_label,
            runtime.policy.device,
            runtime.device.name(),
            runtime.src_ip,
            runtime.policy.zero_copy,
            runtime.policy.queues,
            runtime.policy.components.tpu,
            runtime.policy.components.turbine,
            runtime.policy.components.repair,
            runtime.policy.components.gossip,
            runtime.policy.components.votor,
        );
    }
    Ok(resolved)
}

fn cli_xdp_overrides(matches: &ArgMatches) -> Result<config::CliOverrides, String> {
    let zero_copy = if matches.is_present("xdp_zero_copy") {
        Some(true)
    } else if matches.is_present("no_xdp_zero_copy") {
        Some(false)
    } else {
        None
    };
    Ok(config::CliOverrides {
        no_xdp: matches.is_present("no_xdp"),
        interface: matches.value_of("xdp_interface").map(str::to_string),
        cpu_cores: matches
            .value_of("xdp_cpu_cores")
            .map(|value| {
                parse_cpu_ranges(value)
                    .map_err(|error| format!("invalid --xdp-cpu-cores `{value}`: {error}"))
            })
            .transpose()?,
        zero_copy,
    })
}

#[cfg(all(target_os = "linux", test))]
mod versioned_xdp_tests {
    use {
        super::*,
        crate::{cli::DefaultArgs, commands::run::args::add_args},
        solana_net_utils::multihomed_sockets::BindIpAddrs,
        std::{io::Write as _, net::Ipv6Addr},
    };

    fn write_config(contents: &[u8]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(contents).unwrap();
        file
    }

    fn build_and_validate_config(
        contents: &[u8],
        operation: Operation,
    ) -> Result<Option<ResolvedXdp>, String> {
        let file = write_config(contents);
        let defaults = DefaultArgs::default();
        let app = add_args(clap::App::new("agave-validator"), &defaults);
        let matches = app.get_matches_from(vec![
            "agave-validator",
            "--experimental-config-file",
            file.path().to_str().unwrap(),
        ]);
        let binds = BindIpAddrs::new(vec![Ipv4Addr::UNSPECIFIED.into()]).unwrap();
        let without_xdp = validate_config_file_without_xdp(&matches, &operation);
        let with_xdp = build_xdp_config(&matches, &operation, &binds);
        assert_eq!(
            without_xdp.as_ref().err(),
            with_xdp.as_ref().err(),
            "config loading and initialization should agree across both startup paths"
        );
        with_xdp
    }

    #[test]
    fn test_disabled_xdp_skips_host_resolution() {
        let defaults = DefaultArgs::default();
        let app = add_args(clap::App::new("agave-validator"), &defaults);
        for (case, enabled, flags) in [("CLI", true, vec!["--no-xdp"]), ("file", false, vec![])] {
            // An active policy would fail both CPU and device resolution.
            let file = write_config(
                format!(
                    r#"
schema_version = 1
[xdp]
enabled = {enabled}
[interfaces.primary]
device.name = "nosuchnic0"
[interfaces.primary.xdp]
workers.cpus = [4294967295]
"#
                )
                .as_bytes(),
            );
            let mut args = vec![
                "agave-validator",
                "--experimental-config-file",
                file.path().to_str().unwrap(),
            ];
            args.extend(flags);
            let matches = app.clone().get_matches_from(args);
            for (bind_case, addresses) in [
                ("IPv4", vec![Ipv4Addr::UNSPECIFIED.into()]),
                (
                    "multihoming",
                    vec![
                        Ipv4Addr::new(192, 0, 2, 1).into(),
                        Ipv4Addr::new(192, 0, 2, 2).into(),
                    ],
                ),
                ("IPv6", vec![Ipv6Addr::LOCALHOST.into()]),
            ] {
                let binds = BindIpAddrs::new(addresses).unwrap();
                assert!(
                    build_xdp_config(&matches, &Operation::Run, &binds)
                        .unwrap()
                        .is_none(),
                    "{case}/{bind_case}"
                );
            }
        }
    }

    #[test]
    fn test_policy_is_validated_without_xdp_support() {
        let defaults = DefaultArgs::default();
        let app = add_args(clap::App::new("agave-validator"), &defaults);
        for (contents, expected_error) in [
            (None, None),
            (Some("schema_version = 1"), None),
            (
                Some(
                    r#"
schema_version = 1
[tpu.xdp]
tx.queues = [1]
"#,
                ),
                Some("tpu.xdp.tx.queues references queue(s) 1 not declared"),
            ),
            (
                Some(
                    r#"
schema_version = 1
[xdp]
enabled = false
[tpu.xdp]
tx.queues = [1]
tx.interface = "other"
"#,
                ),
                None,
            ),
        ] {
            let file = contents.map(|contents| write_config(contents.as_bytes()));
            let mut args = vec!["agave-validator"];
            if let Some(file) = &file {
                args.extend(["--experimental-config-file", file.path().to_str().unwrap()]);
            }
            let matches = app.clone().get_matches_from(args);
            let result = validate_config_file_without_xdp(&matches, &Operation::Run);
            match expected_error {
                Some(expected) => {
                    let error = result.unwrap_err();
                    assert!(error.contains(expected), "{contents:?}: {error}");
                }
                None => result.unwrap(),
            }
        }
    }

    #[test]
    fn test_zero_copy_cli_flags_are_parsed_and_conflicts_rejected() {
        use clap::ErrorKind::ArgumentConflict;

        let defaults = DefaultArgs::default();
        let app = add_args(clap::App::new("agave-validator"), &defaults);
        for (flags, expected) in [
            (vec![], Ok(None)),
            (vec!["--xdp-zero-copy"], Ok(Some(true))),
            (vec!["--no-xdp-zero-copy"], Ok(Some(false))),
            (
                vec!["--xdp-zero-copy", "--no-xdp-zero-copy"],
                Err(ArgumentConflict),
            ),
            (
                vec!["--no-xdp", "--no-xdp-zero-copy"],
                Err(ArgumentConflict),
            ),
        ] {
            let result = app
                .clone()
                .get_matches_from_safe(
                    std::iter::once("agave-validator").chain(flags.iter().copied()),
                )
                .map(|matches| cli_xdp_overrides(&matches).unwrap().zero_copy)
                .map_err(|error| error.kind);
            assert_eq!(result, expected, "{flags:?}");
        }
    }

    #[test]
    fn test_unsupported_bind_addresses_are_rejected() {
        let defaults = DefaultArgs::default();
        let app = add_args(clap::App::new("agave-validator"), &defaults);
        let matches = app.get_matches_from(vec!["agave-validator"]);
        for (case, addresses, expected) in [
            (
                "multihoming",
                vec![
                    Ipv4Addr::new(192, 0, 2, 1).into(),
                    Ipv4Addr::new(192, 0, 2, 2).into(),
                ],
                "XDP does not support multiple --bind-address values; select one IPv4 address",
            ),
            (
                "IPv6",
                vec![Ipv6Addr::LOCALHOST.into()],
                "XDP transmit supports IPv4 only; supply an IPv4 --bind-address",
            ),
        ] {
            let binds = BindIpAddrs::new(addresses).unwrap();
            let Err(error) = build_xdp_config(&matches, &Operation::Run, &binds) else {
                panic!("{case} unexpectedly accepted")
            };
            assert_eq!(error, format!("{expected}, or pass --no-xdp"), "{case}");
        }
    }

    #[test]
    fn test_empty_cli_cpu_selection_is_rejected() {
        let defaults = DefaultArgs::default();
        let app = add_args(clap::App::new("agave-validator"), &defaults);
        let matches = app.get_matches_from(vec!["agave-validator", "--xdp-cpu-cores", "5-3"]);
        let binds = BindIpAddrs::new(vec![Ipv4Addr::UNSPECIFIED.into()]).unwrap();
        let Err(error) = build_xdp_config(&matches, &Operation::Run, &binds) else {
            panic!("empty CPU selection unexpectedly accepted")
        };
        assert!(
            error.contains("--xdp-cpu-cores must contain between 1") && error.contains("found 0"),
            "{error}"
        );
    }

    #[test]
    fn test_init_parses_config_without_applying_live_policy() {
        let config = br#"
schema_version = 1

[interfaces.one]
device.name = "eth0"
[interfaces.one.xdp]
zero_copy = false
workers.auto.count = 1

[interfaces.two]
device.name = "eth1"
[interfaces.two.xdp]
zero_copy = false
workers.auto.count = 1
"#;

        assert!(
            build_and_validate_config(config, Operation::Initialize)
                .unwrap()
                .is_none()
        );
        let Err(error) = build_and_validate_config(config, Operation::Run) else {
            panic!("live policy with two interfaces unexpectedly succeeded")
        };
        assert!(error.contains("exactly one"), "{error}");
    }

    #[test]
    fn test_init_rejects_invalid_config_values() {
        let config = br#"
schema_version = "one"
"#;
        let Err(error) = build_and_validate_config(config, Operation::Initialize) else {
            panic!("invalid schema version unexpectedly succeeded")
        };
        assert!(error.contains("non-integer schema_version"), "{error}");
    }

    #[test]
    fn test_missing_device_is_a_targeted_error() {
        let Err(error) = resolve_xdp_device(
            "primary",
            &config::DeviceSelector::Name("nosuchnic0".to_string()),
        ) else {
            panic!("missing device unexpectedly resolved")
        };
        assert!(error.contains("\"nosuchnic0\""), "{error}");
    }

    #[test]
    fn test_source_ipv4_respects_bind_address() {
        let device = NetworkDevice::new("lo").unwrap();
        let explicit = Ipv4Addr::new(192, 0, 2, 1);
        for (bind, expected) in [
            (IpAddr::V4(explicit), Ok(explicit)),
            (IpAddr::V4(Ipv4Addr::UNSPECIFIED), Ok(Ipv4Addr::LOCALHOST)),
            (IpAddr::V6(Ipv6Addr::LOCALHOST), Err("supports IPv4 only")),
        ] {
            let result = resolve_xdp_source_ipv4("primary", &device, bind);
            match expected {
                Ok(expected) => assert_eq!(result.unwrap(), expected, "{bind}"),
                Err(expected) => {
                    let error = result.unwrap_err();
                    assert!(error.contains(expected), "{bind}: {error}");
                }
            }
        }
    }
}
