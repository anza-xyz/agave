use {
    clap::{value_t, value_t_or_exit},
    crossbeam_channel::unbounded,
    solana_clap_utils::input_parsers::keypair_of,
    solana_net_utils::{bind_in_range_with_config, bind_more_with_config, PortRange, SocketConfig},
    solana_sdk::net::DEFAULT_TPU_COALESCE,
    solana_streamer::{nonblocking::quic::DEFAULT_WAIT_FOR_CHUNK_TIMEOUT, streamer::StakedNodes},
    solana_vortexor::{
        cli::{app, DefaultArgs},
        vortexor::{TpuSockets, TpuStreamerConfig, Vortexor},
    },
    std::{
        net::{IpAddr, UdpSocket},
        sync::{atomic::AtomicBool, Arc, RwLock},
        time::Duration,
    },
};

fn bind(bind_ip_addr: IpAddr, port_range: PortRange) -> (u16, UdpSocket) {
    let config = SocketConfig { reuseport: false };
    bind_with_config(bind_ip_addr, port_range, config)
}

fn bind_with_config(
    bind_ip_addr: IpAddr,
    port_range: PortRange,
    config: SocketConfig,
) -> (u16, UdpSocket) {
    bind_in_range_with_config(bind_ip_addr, port_range, config).expect("Failed to bind")
}

pub fn main() {
    let default_args = DefaultArgs::default();
    let solana_version = solana_version::version!();
    let cli_app = app(solana_version, &default_args);
    let matches = cli_app.get_matches();

    let identity_keypair = keypair_of(&matches, "identity").unwrap_or_else(|| {
        clap::Error::with_description(
            "The --identity <KEYPAIR> argument is required",
            clap::ErrorKind::ArgumentNotFound,
        )
        .exit();
    });

    let bind_address = solana_net_utils::parse_host(matches.value_of("bind_address").unwrap())
        .expect("invalid bind_address");
    let max_connections_per_peer = value_t_or_exit!(matches, "max_connections_per_peer", u64);
    let max_tpu_staked_connections = value_t_or_exit!(matches, "max_tpu_staked_connections", u64);
    let max_tpu_unstaked_connections =
        value_t_or_exit!(matches, "max_tpu_unstaked_connections", u64);

    let max_connections_per_ipaddr_per_min =
        value_t_or_exit!(matches, "max_connections_per_ipaddr_per_minute", u64);
    let num_quic_endpoints = value_t_or_exit!(matches, "num_quic_endpoints", u64);
    let tpu_coalesce = value_t!(matches, "tpu_coalesce_ms", u64)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_TPU_COALESCE);

    let dynamic_port_range =
        solana_net_utils::parse_port_range(matches.value_of("dynamic_port_range").unwrap())
            .expect("invalid dynamic_port_range");

    let max_streams_per_ms = value_t_or_exit!(matches, "max_streams_per_ms", u64);

    let config = TpuStreamerConfig {
        tpu_thread_name: "solQuicTpu",
        tpu_metrics_name: "quic_streamer_tpu",
        tpu_fwd_thread_name: "solQuicTpuFwd",
        tpu_fwd_metrics_name: "quic_streamer_tpu_forwards",
        max_connections_per_peer: max_connections_per_peer.try_into().unwrap(),
        max_staked_connections: max_tpu_staked_connections.try_into().unwrap(),
        max_unstaked_connections: max_tpu_unstaked_connections.try_into().unwrap(),
        max_streams_per_ms,
        max_connections_per_ipaddr_per_min,
        wait_for_chunk_timeout: DEFAULT_WAIT_FOR_CHUNK_TIMEOUT,
        sender_coalesce_duration: tpu_coalesce,
    };

    let quic_config = SocketConfig { reuseport: true };

    let (_, tpu_quic) = bind(bind_address, dynamic_port_range);

    let tpu_quic = bind_more_with_config(
        tpu_quic,
        num_quic_endpoints.try_into().unwrap(),
        quic_config.clone(),
    )
    .unwrap();

    let (_, tpu_quic_fwd) = bind(bind_address, dynamic_port_range);

    let tpu_quic_fwd = bind_more_with_config(
        tpu_quic_fwd,
        num_quic_endpoints.try_into().unwrap(),
        quic_config,
    )
    .unwrap();

    let tpu_sockets = TpuSockets {
        tpu_quic,
        tpu_quic_fwd,
    };

    // To be linked with the Tpu sigverify and forwarder service
    let (tpu_sender, _tpu_receiver) = unbounded();
    let (tpu_fwd_sender, _tpu_fwd_receiver) = unbounded();

    // To be linked with StakedNodes service.
    let staked_nodes = Arc::new(RwLock::new(StakedNodes::default()));

    let exit = Arc::new(AtomicBool::new(false));
    let vortexor = Vortexor::new(
        &identity_keypair,
        tpu_sockets,
        tpu_sender,
        tpu_fwd_sender,
        staked_nodes,
        config,
        exit,
    );
    vortexor.join().unwrap();
}
