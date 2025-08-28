use {
    crate::{
        args::{LayeredArg, Result},
        config_file::ValidatorConfig,
    },
    clap::{App, Arg, ArgMatches, Error},
    solana_clap_utils::{
        hidden_unless_forced, input_parsers::parse_cpu_ranges,
        input_validators::validate_cpu_ranges,
    },
    solana_turbine::xdp::XdpConfig,
};

impl LayeredArg for XdpConfig {
    fn declare_cli_args<'a, 'b>(app: App<'a, 'b>) -> App<'a, 'b> {
        app.arg(
            Arg::with_name("retransmit_xdp_interface")
                .hidden(hidden_unless_forced())
                .long("experimental-retransmit-xdp-interface")
                .takes_value(true)
                .value_name("INTERFACE")
                .requires("retransmit_xdp_cpu_cores")
                .help("EXPERIMENTAL: The network interface to use for XDP retransmit"),
        )
        .arg(
            Arg::with_name("retransmit_xdp_cpu_cores")
                .hidden(hidden_unless_forced())
                .long("experimental-retransmit-xdp-cpu-cores")
                .takes_value(true)
                .value_name("CPU_LIST")
                .validator(|value| {
                    validate_cpu_ranges(value, "--experimental-retransmit-xdp-cpu-cores")
                })
                .help("EXPERIMENTAL: Enable XDP retransmit on the specified CPU cores"),
        )
        .arg(
            Arg::with_name("retransmit_xdp_zero_copy")
                .hidden(hidden_unless_forced())
                .long("experimental-retransmit-xdp-zero-copy")
                .takes_value(false)
                .requires("retransmit_xdp_cpu_cores")
                .help("EXPERIMENTAL: Enable XDP zero copy. Requires hardware support"),
        )
    }

    fn from_clap_arg_matches(matches: &ArgMatches) -> Option<Result<Self>> {
        let xdp_interface = matches.value_of("retransmit_xdp_interface");
        let xdp_zero_copy = matches.is_present("retransmit_xdp_zero_copy");
        let cpus = match parse_cpu_ranges(matches.value_of("retransmit_xdp_cpu_cores")?) {
            Ok(cores) => cores,
            Err(error) => return Some(Err(Error::from(error))),
        };

        Some(Ok(XdpConfig::new(xdp_interface, cpus, xdp_zero_copy)))
    }

    fn from_validator_config(config: &ValidatorConfig) -> Option<Result<Self>> {
        let interface = config
            .net
            .xdp
            .interface
            .as_ref()
            .and_then(|ifaces| ifaces.first().map(|iface| iface.interface.clone()));

        Some(Ok(XdpConfig::new(
            interface,
            config.net.xdp.cpus.as_ref().map(|cpus| cpus.0.clone())?,
            config.net.xdp.zero_copy.unwrap_or_default(),
        )))
    }
}
