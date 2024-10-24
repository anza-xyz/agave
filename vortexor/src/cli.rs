use {
    crate::vortexor::{MAX_QUIC_CONNECTIONS_PER_PEER, NUM_QUIC_ENDPOINTS},
    clap::{crate_description, crate_name, App, AppSettings, Arg},
    solana_clap_utils::input_validators::{is_keypair_or_ask_keyword, is_parsable},
    solana_net_utils::VALIDATOR_PORT_RANGE,
    solana_streamer::{
        nonblocking::quic::{
            DEFAULT_MAX_CONNECTIONS_PER_IPADDR_PER_MINUTE, DEFAULT_MAX_STREAMS_PER_MS,
        },
        quic::{MAX_STAKED_CONNECTIONS, MAX_UNSTAKED_CONNECTIONS},
    },
};

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
            max_connections_per_peer: MAX_QUIC_CONNECTIONS_PER_PEER.to_string(),
            max_tpu_staked_connections: MAX_STAKED_CONNECTIONS.to_string(),
            max_tpu_unstaked_connections: MAX_UNSTAKED_CONNECTIONS.to_string(),
            max_fwd_staked_connections: MAX_STAKED_CONNECTIONS
                .saturating_add(MAX_UNSTAKED_CONNECTIONS)
                .to_string(),
            max_fwd_unstaked_connections: 0.to_string(),
            max_streams_per_ms: DEFAULT_MAX_STREAMS_PER_MS.to_string(),
            max_connections_per_ipaddr_per_min: DEFAULT_MAX_CONNECTIONS_PER_IPADDR_PER_MINUTE
                .to_string(),
            num_quic_endpoints: NUM_QUIC_ENDPOINTS.to_string(),
        }
    }
}

pub fn app<'a>(version: &'a str, default_args: &'a DefaultArgs) -> App<'a, 'a> {
    return App::new(crate_name!())
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
            Arg::with_name("max_connections_per_peer")
                .long("max-connections-per-peer")
                .takes_value(true)
                .default_value(&default_args.max_connections_per_peer)
                .validator(is_parsable::<u32>)
                .help("Controls the max concurrent connection per IpAddr."),
        )
        .arg(
            Arg::with_name("max_tpu_staked_connections")
                .long("max-tpu-staked-connections")
                .takes_value(true)
                .default_value(&default_args.max_tpu_staked_connections)
                .validator(is_parsable::<u32>)
                .help("Controls the max concurrent connection per IpAddr."),
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
                .help("The number of QUIC endpoints used for TPU and TPU-Forward. It can be increased to \
                       increase network ingest throughput, at the expense of higher CPU and general \
                       validator load."),
        );
}
