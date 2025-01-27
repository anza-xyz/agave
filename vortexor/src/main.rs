use {
    clap::{value_t, value_t_or_exit},
    crossbeam_channel::bounded,
    solana_clap_utils::input_parsers::keypair_of,
    solana_core::banking_trace::BankingTracer,
    solana_sdk::net::DEFAULT_TPU_COALESCE,
    solana_streamer::streamer::StakedNodes,
    solana_vortexor::{
        cli::{app, DefaultArgs},
        vortexor::Vortexor,
    },
    std::{
        sync::{atomic::AtomicBool, Arc, RwLock},
        time::Duration,
    },
};

const DEFAULT_CHANNEL_SIZE: usize = 100_000;

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
    let max_fwd_staked_connections = value_t_or_exit!(matches, "max_fwd_staked_connections", u64);
    let max_fwd_unstaked_connections =
        value_t_or_exit!(matches, "max_fwd_unstaked_connections", u64);

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
    let exit = Arc::new(AtomicBool::new(false));
    // To be linked with the Tpu sigverify and forwarder service
    let (tpu_sender, tpu_receiver) = bounded(DEFAULT_CHANNEL_SIZE);
    let (tpu_fwd_sender, _tpu_fwd_receiver) = bounded(DEFAULT_CHANNEL_SIZE);

    let tpu_sockets =
        Vortexor::create_tpu_sockets(bind_address, dynamic_port_range, num_quic_endpoints);

    let (banking_tracer, _) = BankingTracer::new(
        None, // Not interesed in banking tracing
    )
    .unwrap();

    // The _non_vote_receiver will forward the verified transactions to its configured validator
    let (non_vote_sender, _non_vote_receiver) = banking_tracer.create_channel_non_vote();

    let sigverify_stage = Vortexor::create_sigverify_stage(tpu_receiver, non_vote_sender);

    // To be linked with StakedNodes service.
    let staked_nodes = Arc::new(RwLock::new(StakedNodes::default()));

    let vortexor = Vortexor::create_vortexor(
        tpu_sockets,
        staked_nodes,
        tpu_sender,
        tpu_fwd_sender,
        max_connections_per_peer,
        max_tpu_staked_connections,
        max_tpu_unstaked_connections,
        max_fwd_staked_connections,
        max_fwd_unstaked_connections,
        max_streams_per_ms,
        max_connections_per_ipaddr_per_min,
        tpu_coalesce,
        &identity_keypair,
        exit,
    );
    vortexor.join().unwrap();
    sigverify_stage.join().unwrap();
}
