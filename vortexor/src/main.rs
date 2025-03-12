use {
    clap::{crate_name, value_t, value_t_or_exit, values_t},
    crossbeam_channel::bounded,
    log::*,
    solana_clap_utils::input_parsers::keypair_of,
    solana_core::banking_trace::BankingTracer,
    solana_logger::redirect_stderr_to_file,
    solana_net_utils::{bind_in_range_with_config, SocketConfig},
    solana_sdk::{net::DEFAULT_TPU_COALESCE, signer::Signer},
    solana_streamer::streamer::StakedNodes,
    solana_vortexor::{
        cli::{app, DefaultArgs},
        sender::{
            PacketBatchSender, DEFAULT_BATCH_SIZE, DEFAULT_RECV_TIMEOUT,
            DEFAULT_SENDER_THREADS_COUNT,
        },
        vortexor::Vortexor,
    },
    std::{
        collections::HashSet,
        env,
        sync::{atomic::AtomicBool, Arc, RwLock},
        time::Duration,
    },
};

const DEFAULT_CHANNEL_SIZE: usize = 100_000;

pub fn main() {
    solana_logger::setup();

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

    let logfile = {
        let logfile = matches
            .value_of("logfile")
            .map(|s| s.into())
            .unwrap_or_else(|| format!("solana-vortexor-{}.log", identity_keypair.pubkey()));

        if logfile == "-" {
            None
        } else {
            println!("log file: {logfile}");
            Some(logfile)
        }
    };
    let _logger_thread = redirect_stderr_to_file(logfile);

    info!("{} {solana_version}", crate_name!());
    info!(
        "Starting vortexor {} with: {:#?}",
        identity_keypair.pubkey(),
        std::env::args_os()
    );

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

    let config = SocketConfig::default().reuseport(false);

    let sender_socket =
        bind_in_range_with_config(bind_address, dynamic_port_range, config).unwrap();

    // The non_vote_receiver will forward the verified transactions to its configured validator
    let (non_vote_sender, non_vote_receiver) = banking_tracer.create_channel_non_vote();

    let destinations = values_t!(matches, "destination", String)
        .unwrap_or_default()
        .into_iter()
        .map(|destination| {
            solana_net_utils::parse_host_port(&destination).unwrap_or_else(|e| {
                panic!("Failed to parse destination address: {e}");
            })
        })
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();

    info!("Creating the PacketBatchSender: at address: {:?} for the following initial destinations: {destinations:?}",
        sender_socket.1.local_addr());

    let destinations = Arc::new(RwLock::new(destinations));
    let packet_sender = PacketBatchSender::new(
        sender_socket.1,
        non_vote_receiver,
        DEFAULT_SENDER_THREADS_COUNT,
        DEFAULT_BATCH_SIZE,
        DEFAULT_RECV_TIMEOUT,
        destinations,
    );

    info!("Creating the SigVerifier");
    let sigverify_stage = Vortexor::create_sigverify_stage(tpu_receiver, non_vote_sender);

    // To be linked with StakedNodes service.
    let staked_nodes = Arc::new(RwLock::new(StakedNodes::default()));

    info!(
        "Creating the Vortexor. The tpu socket is: {:?}, tpu_fwd: {:?}",
        tpu_sockets.tpu_quic[0].local_addr(),
        tpu_sockets.tpu_quic_fwd[0].local_addr()
    );

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
    packet_sender.join().unwrap();
}
