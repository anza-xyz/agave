use {
    clap::{crate_description, crate_name, Arg, ArgAction, ColorChoice, Command},
    solana_net_utils::{MINIMUM_VALIDATOR_PORT_RANGE_WIDTH, VALIDATOR_PORT_RANGE},
    solana_sdk::quic::QUIC_PORT_OFFSET,
    solana_streamer::quic::{
        DEFAULT_MAX_CONNECTIONS_PER_IPADDR_PER_MINUTE, DEFAULT_MAX_STAKED_CONNECTIONS,
        DEFAULT_MAX_STREAMS_PER_MS, DEFAULT_MAX_UNSTAKED_CONNECTIONS,
    },
    std::path::PathBuf,
};

pub const DEFAULT_MAX_QUIC_CONNECTIONS_PER_PEER: usize = 8;
pub const DEFAULT_NUM_QUIC_ENDPOINTS: usize = 8;

pub struct DefaultArgs {
    pub bind_address: String,
    pub dynamic_port_range: String,
    pub max_connections_per_peer: String,
    pub max_tpu_staked_connections: String,
    pub max_tpu_unstaked_connections: String,
    pub max_fwd_staked_connections: String,
    pub max_fwd_unstaked_connections: String,
    pub max_streams_per_ms: String,
    pub max_connections_per_ipaddr_per_min: String,
    pub num_quic_endpoints: String,
}

impl Default for DefaultArgs {
    fn default() -> Self {
        Self {
            bind_address: "0.0.0.0".to_string(),
            dynamic_port_range: format!("{}-{}", VALIDATOR_PORT_RANGE.0, VALIDATOR_PORT_RANGE.1),
            max_connections_per_peer: DEFAULT_MAX_QUIC_CONNECTIONS_PER_PEER.to_string(),
            max_tpu_staked_connections: DEFAULT_MAX_STAKED_CONNECTIONS.to_string(),
            max_tpu_unstaked_connections: DEFAULT_MAX_UNSTAKED_CONNECTIONS.to_string(),
            max_fwd_staked_connections: DEFAULT_MAX_STAKED_CONNECTIONS
                .saturating_add(DEFAULT_MAX_UNSTAKED_CONNECTIONS)
                .to_string(),
            max_fwd_unstaked_connections: 0.to_string(),
            max_streams_per_ms: DEFAULT_MAX_STREAMS_PER_MS.to_string(),
            max_connections_per_ipaddr_per_min: DEFAULT_MAX_CONNECTIONS_PER_IPADDR_PER_MINUTE
                .to_string(),
            num_quic_endpoints: DEFAULT_NUM_QUIC_ENDPOINTS.to_string(),
        }
    }
}

fn port_range_validator(port_range: &str) -> Result<(u16, u16), String> {
    if let Some((start, end)) = solana_net_utils::parse_port_range(port_range) {
        if end.saturating_sub(start) < MINIMUM_VALIDATOR_PORT_RANGE_WIDTH {
            Err(format!(
                "Port range is too small.  Try --dynamic-port-range {}-{}",
                start,
                start.saturating_add(MINIMUM_VALIDATOR_PORT_RANGE_WIDTH)
            ))
        } else if end.checked_add(QUIC_PORT_OFFSET).is_none() {
            Err("Invalid dynamic_port_range.".to_string())
        } else {
            Ok((start, end))
        }
    } else {
        Err("Invalid port range".to_string())
    }
}

pub fn command(version: &str, default_args: DefaultArgs) -> Command {
    // The default values need to be static:
    let version_static: &'static str = Box::leak(version.to_string().into_boxed_str());
    let bind_address_static: &'static str = Box::leak(default_args.bind_address.into_boxed_str());
    let port_range_static: &'static str =
        Box::leak(default_args.dynamic_port_range.into_boxed_str());
    let max_connections_per_peer_static: &'static str =
        Box::leak(default_args.max_connections_per_peer.into_boxed_str());
    let max_tpu_staked_connections_static: &'static str =
        Box::leak(default_args.max_tpu_staked_connections.into_boxed_str());
    let max_tpu_unstaked_connections_static: &'static str =
        Box::leak(default_args.max_tpu_unstaked_connections.into_boxed_str());
    let max_fwd_staked_connections_static: &'static str =
        Box::leak(default_args.max_fwd_staked_connections.into_boxed_str());
    let max_fwd_unstaked_connections_static: &'static str =
        Box::leak(default_args.max_fwd_unstaked_connections.into_boxed_str());
    let max_connections_per_ipaddr_per_min_static: &'static str = Box::leak(
        default_args
            .max_connections_per_ipaddr_per_min
            .into_boxed_str(),
    );
    let num_quic_endpoints_static: &'static str =
        Box::leak(default_args.num_quic_endpoints.into_boxed_str());
    let max_streams_per_ms_static: &'static str =
        Box::leak(default_args.max_streams_per_ms.into_boxed_str());

    Command::new(crate_name!())
        .about(crate_description!())
        .version(version_static)
        .infer_subcommands(true)
        .color(ColorChoice::Auto)
        .arg(
            Arg::new("identity")
                .long("identity")
                .value_name("KEYPAIR")
                .num_args(1)
                .required(true)
                .value_parser(clap::value_parser!(PathBuf))
                .help("Vortexor identity keypair"),
        )
        .arg(
            Arg::new("bind_address")
                .long("bind-address")
                .value_name("HOST")
                .num_args(1)
                .value_parser(solana_net_utils::parse_host)
                .default_value(bind_address_static)
                .help("IP address to bind the validator ports"),
        )
        .arg(
            Arg::new("dynamic_port_range")
                .long("dynamic-port-range")
                .value_name("MIN_PORT-MAX_PORT")
                .num_args(1)
                .default_value(port_range_static)
                .value_parser(port_range_validator)
                .help("Range to use for dynamically assigned ports"),
        )
        .arg(
            Arg::new("max_connections_per_peer")
                .long("max-connections-per-peer")
                .num_args(1)
                .default_value(max_connections_per_peer_static)
                .value_parser(clap::value_parser!(u64))
                .help("Controls the max concurrent connections per IpAddr."),
        )
        .arg(
            Arg::new("max_tpu_staked_connections")
                .long("max-tpu-staked-connections")
                .num_args(1)
                .default_value(max_tpu_staked_connections_static)
                .value_parser(clap::value_parser!(u64))
                .help("Controls the max concurrent connections for TPU from staked nodes."),
        )
        .arg(
            Arg::new("max_tpu_unstaked_connections")
                .long("max-tpu-unstaked-connections")
                .num_args(1)
                .default_value(max_tpu_unstaked_connections_static)
                .value_parser(clap::value_parser!(u64))
                .help("Controls the max concurrent connections fort TPU from unstaked nodes."),
        )
        .arg(
            Arg::new("max_fwd_staked_connections")
                .long("max-fwd-staked-connections")
                .num_args(1)
                .default_value(max_fwd_staked_connections_static)
                .value_parser(clap::value_parser!(u64))
                .help("Controls the max concurrent connections for TPU-forward from staked nodes."),
        )
        .arg(
            Arg::new("max_fwd_unstaked_connections")
                .long("max-fwd-unstaked-connections")
                .num_args(1)
                .default_value(max_fwd_unstaked_connections_static)
                .value_parser(clap::value_parser!(u64))
                .help("Controls the max concurrent connections for TPU-forward from unstaked nodes."),
        )
        .arg(
            Arg::new("max_connections_per_ipaddr_per_minute")
                .long("max-connections-per-ipaddr-per-minute")
                .num_args(1)
                .default_value(max_connections_per_ipaddr_per_min_static)
                .value_parser(clap::value_parser!(u64))
                .help("Controls the rate of the clients connections per IpAddr per minute."),
        )
        .arg(
            Arg::new("num_quic_endpoints")
                .long("num-quic-endpoints")
                .num_args(1)
                .default_value(num_quic_endpoints_static)
                .value_parser(clap::value_parser!(u64))
                .help("The number of QUIC endpoints used for TPU and TPU-Forward. It can be increased to \
                       increase network ingest throughput, at the expense of higher CPU and general \
                       validator load."),
        )
        .arg(
            Arg::new("max_streams_per_ms")
                .long("max-streams-per-ms")
                .num_args(1)
                .default_value(max_streams_per_ms_static)
                .value_parser(clap::value_parser!(u64))
                .help("Max streams per second for a streamer."),
        )
        .arg(
            Arg::new("tpu_coalesce_ms")
                .long("tpu-coalesce-ms")
                .value_name("MILLISECS")
                .num_args(1)
                .value_parser(clap::value_parser!(u64))
                .help("Milliseconds to wait in the TPU receiver for packet coalescing."),
        )
        .arg(
            Arg::new("logfile")
                .long("log")
                .value_name("FILE")
                .num_args(1)
                .help(
                    "Redirect logging to the specified file, '-' for standard error. Sending the \
                     SIGUSR1 signal to the vortexor process will cause it to re-open the log file.",
                ),
        )
        .arg(
            Arg::new("destination")
                .long("destination")
                .value_name("HOST:PORT")
                .action(ArgAction::Append)
                .num_args(1)
                .value_parser(solana_net_utils::parse_host_port)
                .help("The destination validator address to which the vortexor will forward transactions."),
        )
}
