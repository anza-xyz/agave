use {
    clap::{builder::ValueParser, crate_description, crate_name, ArgAction, ColorChoice, Parser},
    solana_net_utils::{MINIMUM_VALIDATOR_PORT_RANGE_WIDTH, VALIDATOR_PORT_RANGE},
    solana_sdk::{net::DEFAULT_TPU_COALESCE, quic::QUIC_PORT_OFFSET},
    solana_streamer::quic::{
        DEFAULT_MAX_CONNECTIONS_PER_IPADDR_PER_MINUTE, DEFAULT_MAX_STAKED_CONNECTIONS,
        DEFAULT_MAX_STREAMS_PER_MS, DEFAULT_MAX_UNSTAKED_CONNECTIONS,
    },
    std::{
        net::{IpAddr, SocketAddr},
        path::PathBuf,
    },
    url::Url,
};

pub const DEFAULT_MAX_QUIC_CONNECTIONS_PER_PEER: usize = 8;
pub const DEFAULT_NUM_QUIC_ENDPOINTS: usize = 8;

fn parse_port_range(port_range: &str) -> Result<(u16, u16), String> {
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

fn get_version() -> &'static str {
    let version = solana_version::version!();
    let version_static: &'static str = Box::leak(version.to_string().into_boxed_str());
    version_static
}

fn get_default_port_range() -> &'static str {
    let range = format!("{}-{}", VALIDATOR_PORT_RANGE.0, VALIDATOR_PORT_RANGE.1);
    let range: &'static str = Box::leak(range.into_boxed_str());
    range
}

fn get_default_tpu_coalesce_ms() -> &'static str {
    let coalesce = DEFAULT_TPU_COALESCE.as_millis().to_string();
    let coalesce: &'static str = Box::leak(coalesce.into_boxed_str());
    coalesce
}

/// returns a parser which can validate input URL based on specified schemes.
fn parse_url_with_scheme(expected_schemes: &'static [&'static str]) -> ValueParser {
    ValueParser::from(move |input: &str| {
        // Attempt to parse the input as a URL
        let parsed_url =
            Url::parse(input).map_err(|e| format!("Invalid URL '{}': {}", input, e))?;

        // Check the scheme of the URL
        if expected_schemes.contains(&parsed_url.scheme()) {
            Ok(parsed_url)
        } else {
            Err(format!(
                "Invalid scheme: {}. Must be one of: {}.",
                parsed_url.scheme(),
                expected_schemes.join(", ")
            ))
        }
    })
}

#[derive(Parser)]
#[command(name=crate_name!(),version=get_version(), about=crate_description!(),
    long_about = None, color=ColorChoice::Auto)]
pub struct Cli {
    /// Vortexor identity keypair
    #[arg(long, value_name = "KEYPAIR")]
    pub identity: PathBuf,

    /// IP address to bind the vortexor ports
    #[arg(long, default_value = "0.0.0.0", value_name = "HOST")]
    pub bind_address: IpAddr,

    /// The destination validator address to which the vortexor will forward transactions.
    #[arg(long, value_name = "HOST:PORT", action = ArgAction::Append)]
    pub destination: Vec<SocketAddr>,

    /// Range to use for dynamically assigned ports
    #[arg(long, value_parser = parse_port_range, value_name = "MIN_PORT-MAX_PORT", default_value = get_default_port_range())]
    pub dynamic_port_range: (u16, u16),

    /// Controls the max concurrent connections per IpAddr.
    #[arg(long, default_value_t = DEFAULT_MAX_QUIC_CONNECTIONS_PER_PEER)]
    pub max_connections_per_peer: usize,

    /// Controls the max concurrent connections for TPU from staked nodes.
    #[arg(long, default_value_t = DEFAULT_MAX_STAKED_CONNECTIONS)]
    pub max_tpu_staked_connections: usize,

    /// Controls the max concurrent connections fort TPU from unstaked nodes.
    #[arg(long, default_value_t = DEFAULT_MAX_UNSTAKED_CONNECTIONS)]
    pub max_tpu_unstaked_connections: usize,

    /// Controls the max concurrent connections for TPU-forward from staked nodes.
    #[arg(long, default_value_t = DEFAULT_MAX_STAKED_CONNECTIONS)]
    pub max_fwd_staked_connections: usize,

    /// Controls the max concurrent connections for TPU-forward from unstaked nodes.
    #[arg(long, default_value_t = 0)]
    pub max_fwd_unstaked_connections: usize,

    /// Controls the rate of the clients connections per IpAddr per minute.
    #[arg(long, default_value_t = DEFAULT_MAX_CONNECTIONS_PER_IPADDR_PER_MINUTE)]
    pub max_connections_per_ipaddr_per_minute: u64,

    /// The number of QUIC endpoints used for TPU and TPU-Forward. It can be increased to
    /// increase network ingest throughput, at the expense of higher CPU and general
    /// validator load.
    #[arg(long, default_value_t = DEFAULT_NUM_QUIC_ENDPOINTS)]
    pub num_quic_endpoints: usize,

    /// Max streams per second for a streamer.
    #[arg(long, default_value_t = DEFAULT_MAX_STREAMS_PER_MS)]
    pub max_streams_per_ms: u64,

    /// Milliseconds to wait in the TPU receiver for packet coalescing.
    #[arg(long, default_value = get_default_tpu_coalesce_ms())]
    pub tpu_coalesce_ms: u64,

    /// Redirect logging to the specified file, '-' for standard error. Sending the
    /// SIGUSR1 signal to the vortexor process will cause it to re-open the log file.
    #[arg(long="log", value_name = "FILE", value_parser = clap::value_parser!(String))]
    pub logfile: Option<String>,

    /// The address(es) of RPC server that the vortexor will connect to obtain stake and slot info.
    #[arg(long="rpc-server", value_parser = parse_url_with_scheme(&["http", "https"]), value_name = "URL")]
    pub rpc_servers: Vec<Url>,

    /// The address (es) of websocket server to which the vortexor will connect to obtain stake and slot info.
    /// If multiple rpc servers are set, the count of websocket servers must
    /// match that of the rpc servers.
    #[arg(long="websocket-server", value_parser = parse_url_with_scheme(&["ws", "wss"]), value_name = "URL")]
    pub websocket_servers: Vec<Url>,
}
