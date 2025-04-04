use {
    clap::{crate_name, Parser},
    crossbeam_channel::bounded,
    log::*,
    solana_core::banking_trace::BankingTracer,
    solana_logger::redirect_stderr_to_file,
    solana_net_utils::{bind_in_range_with_config, SocketConfig},
    solana_sdk::{signature::read_keypair_file, signer::Signer},
    solana_streamer::streamer::StakedNodes,
    solana_vortexor::{
        cli::Cli,
        rpc_load_balancer::RpcLoadBalancer,
        sender::{
            PacketBatchSender, DEFAULT_BATCH_SIZE, DEFAULT_RECV_TIMEOUT,
            DEFAULT_SENDER_THREADS_COUNT,
        },
        stake_updater::StakeUpdater,
        vortexor::Vortexor,
    },
    std::{
        collections::HashMap,
        env,
        net::IpAddr,
        sync::{atomic::AtomicBool, Arc, RwLock},
        time::Duration,
    },
};

const DEFAULT_CHANNEL_SIZE: usize = 100_000;

pub fn main() {
    solana_logger::setup();

    let args = Cli::parse();
    let solana_version = solana_version::version!();
    let identity = args.identity;

    let identity_keypair = read_keypair_file(identity).unwrap_or_else(|error| {
        clap::Error::raw(
            clap::error::ErrorKind::InvalidValue,
            format!("The --identity <KEYPAIR> argument is required, error: {error}"),
        )
        .exit();
    });

    let logfile = {
        let logfile = args
            .logfile
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

    let bind_address: &IpAddr = &args.bind_address;
    let max_connections_per_peer = args.max_connections_per_peer;
    let max_tpu_staked_connections = args.max_tpu_staked_connections;
    let max_fwd_staked_connections = args.max_fwd_staked_connections;
    let max_fwd_unstaked_connections = args.max_fwd_unstaked_connections;

    let max_tpu_unstaked_connections = args.max_tpu_unstaked_connections;

    let max_connections_per_ipaddr_per_min = args.max_connections_per_ipaddr_per_minute;
    let num_quic_endpoints = args.num_quic_endpoints;
    let tpu_coalesce = Duration::from_millis(args.tpu_coalesce_ms);
    let dynamic_port_range = args.dynamic_port_range;

    let max_streams_per_ms = args.max_streams_per_ms;
    let exit = Arc::new(AtomicBool::new(false));
    // To be linked with the Tpu sigverify and forwarder service
    let (tpu_sender, tpu_receiver) = bounded(DEFAULT_CHANNEL_SIZE);
    let (tpu_fwd_sender, _tpu_fwd_receiver) = bounded(DEFAULT_CHANNEL_SIZE);

    let tpu_sockets =
        Vortexor::create_tpu_sockets(*bind_address, dynamic_port_range, num_quic_endpoints);

    let (banking_tracer, _) = BankingTracer::new(
        None, // Not interesed in banking tracing
    )
    .unwrap();

    let config = SocketConfig::default().reuseport(false);

    let sender_socket =
        bind_in_range_with_config(*bind_address, dynamic_port_range, config).unwrap();

    // The non_vote_receiver will forward the verified transactions to its configured validator
    let (non_vote_sender, non_vote_receiver) = banking_tracer.create_channel_non_vote();
    let destinations = args.destination;

    let rpc_servers = args.rpc_servers;
    let websocket_servers = args.websocket_servers;

    if rpc_servers.len() != websocket_servers.len() {
        clap::Error::raw(
            clap::error::ErrorKind::InvalidValue,
            "There must be equal number of rpc-server(s) and websocket-server(s).",
        )
        .exit();
    }
    let servers = rpc_servers
        .into_iter()
        .zip(websocket_servers)
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
    let stake_map = Arc::new(HashMap::new());
    let staked_nodes_overrides = HashMap::new();

    let staked_nodes = Arc::new(RwLock::new(StakedNodes::new(
        stake_map,
        staked_nodes_overrides,
    )));

    let (rpc_load_balancer, _slot_receiver) = RpcLoadBalancer::new(&servers, &exit);
    let rpc_load_balancer = Arc::new(rpc_load_balancer);

    let staked_nodes_updater_service = StakeUpdater::new(
        exit.clone(),
        rpc_load_balancer.clone(),
        staked_nodes.clone(),
    );

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
    staked_nodes_updater_service.join().unwrap();
}
