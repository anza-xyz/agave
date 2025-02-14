use {
    clap::{crate_description, crate_name, App, AppSettings, Arg},
    solana_clap_utils::input_validators::{is_keypair_or_ask_keyword, is_parsable},
    solana_net_utils::{MINIMUM_VALIDATOR_PORT_RANGE_WIDTH, VALIDATOR_PORT_RANGE},
    solana_sdk::quic::QUIC_PORT_OFFSET,
    solana_streamer::quic::{
        DEFAULT_MAX_CONNECTIONS_PER_IPADDR_PER_MINUTE, DEFAULT_MAX_STAKED_CONNECTIONS,
        DEFAULT_MAX_STREAMS_PER_MS, DEFAULT_MAX_UNSTAKED_CONNECTIONS,
    },
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

fn port_range_validator(port_range: String) -> Result<(), String> {
    if let Some((start, end)) = solana_net_utils::parse_port_range(&port_range) {
        if end.saturating_sub(start) < MINIMUM_VALIDATOR_PORT_RANGE_WIDTH {
            Err(format!(
                "Port range is too small.  Try --dynamic-port-range {}-{}",
                start,
                start.saturating_add(MINIMUM_VALIDATOR_PORT_RANGE_WIDTH)
            ))
        } else if end.checked_add(QUIC_PORT_OFFSET).is_none() {
            Err("Invalid dynamic_port_range.".to_string())
        } else {
            Ok(())
        }
    } else {
        Err("Invalid port range".to_string())
    }
}

pub fn app<'a>(version: &'a str, default_args: &'a DefaultArgs) -> App<'a, 'a> {
    App::new(crate_name!())
        .about(crate_description!())
        .version(version)
        .global_setting(AppSettings::ColoredHelp)
        .global_setting(AppSettings::InferSubcommands)
        .global_setting(AppSettings::UnifiedHelpMessage)
        .global_setting(AppSettings::VersionlessSubcommands)
        .arg(
            Arg::with_name("identity")
                .short("i")
                .long("identity")
                .value_name("KEYPAIR")
                .takes_value(true)
                .validator(is_keypair_or_ask_keyword)
                .help("Vortexor identity keypair"),
        )
        .arg(
            Arg::with_name("bind_address")
                .long("bind-address")
                .value_name("HOST")
                .takes_value(true)
                .validator(solana_net_utils::is_host)
                .default_value(&default_args.bind_address)
                .help("IP address to bind the validator ports"),
        )
        .arg(
            Arg::with_name("dynamic_port_range")
                .long("dynamic-port-range")
                .value_name("MIN_PORT-MAX_PORT")
                .takes_value(true)
                .default_value(&default_args.dynamic_port_range)
                .validator(port_range_validator)
                .help("Range to use for dynamically assigned ports"),
        )
        .arg(
            Arg::with_name("max_connections_per_peer")
                .long("max-connections-per-peer")
                .takes_value(true)
                .default_value(&default_args.max_connections_per_peer)
                .validator(is_parsable::<u32>)
                .help("Controls the max concurrent connections per IpAddr."),
        )
        .arg(
            Arg::with_name("max_tpu_staked_connections")
                .long("max-tpu-staked-connections")
                .takes_value(true)
                .default_value(&default_args.max_tpu_staked_connections)
                .validator(is_parsable::<u32>)
                .help("Controls the max concurrent connections for TPU from staked nodes."),
        )
        .arg(
            Arg::with_name("max_tpu_unstaked_connections")
                .long("max-tpu-unstaked-connections")
                .takes_value(true)
                .default_value(&default_args.max_tpu_unstaked_connections)
                .validator(is_parsable::<u32>)
                .help("Controls the max concurrent connections fort TPU from unstaked nodes."),
        )
        .arg(
            Arg::with_name("max_fwd_staked_connections")
                .long("max-fwd-staked-connections")
                .takes_value(true)
                .default_value(&default_args.max_fwd_staked_connections)
                .validator(is_parsable::<u32>)
                .help("Controls the max concurrent connections for TPU-forward from staked nodes."),
        )
        .arg(
            Arg::with_name("max_fwd_unstaked_connections")
                .long("max-fwd-unstaked-connections")
                .takes_value(true)
                .default_value(&default_args.max_fwd_unstaked_connections)
                .validator(is_parsable::<u32>)
                .help(
                    "Controls the max concurrent connections for TPU-forward from unstaked nodes.",
                ),
        )
        .arg(
            Arg::with_name("max_connections_per_ipaddr_per_minute")
                .long("max-connections-per-ipaddr-per-minute")
                .takes_value(true)
                .default_value(&default_args.max_connections_per_ipaddr_per_min)
                .validator(is_parsable::<u32>)
                .help("Controls the rate of the clients connections per IpAddr per minute."),
        )
        .arg(
            Arg::with_name("num_quic_endpoints")
                .long("num-quic-endpoints")
                .takes_value(true)
                .default_value(&default_args.num_quic_endpoints)
                .validator(is_parsable::<usize>)
                .help(
                    "The number of QUIC endpoints used for TPU and TPU-Forward. It can be \
                     increased to increase network ingest throughput, at the expense of higher \
                     CPU and general validator load.",
                ),
        )
        .arg(
            Arg::with_name("max_streams_per_ms")
                .long("max-streams-per-ms")
                .takes_value(true)
                .default_value(&default_args.max_streams_per_ms)
                .validator(is_parsable::<usize>)
                .help("Max streams per second for a streamer."),
        )
        .arg(
            Arg::with_name("tpu_coalesce_ms")
                .long("tpu-coalesce-ms")
                .value_name("MILLISECS")
                .takes_value(true)
                .validator(is_parsable::<u64>)
                .help("Milliseconds to wait in the TPU receiver for packet coalescing."),
        )
}
