#![allow(clippy::arithmetic_side_effects)]

use {
    clap::{crate_description, crate_name, value_t, value_t_or_exit, App, Arg},
    crossbeam_channel::unbounded,
    solana_clap_utils::{input_parsers::keypair_of, input_validators::is_keypair_or_ask_keyword},
    solana_client::connection_cache::ConnectionCache,
    solana_connection_cache::client_connection::ClientConnection,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_message::Message,
    solana_net_utils::{
        bind_to_unspecified,
        sockets::{multi_bind_in_range_with_config, SocketConfiguration as SocketConfig},
    },
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    solana_streamer::{
        nonblocking::swqos::SwQosConfig,
        packet::PacketBatchRecycler,
        quic::{
            spawn_stake_wighted_qos_server, QuicStreamerConfig,
            DEFAULT_MAX_QUIC_CONNECTIONS_PER_UNSTAKED_PEER, DEFAULT_MAX_STAKED_CONNECTIONS,
        },
        streamer::{receiver, PacketBatchReceiver, StakedNodes, StreamerReceiveStats},
    },
    solana_transaction::Transaction,
    solana_vote_program::{vote_instruction, vote_state::Vote},
    std::{
        cmp::max,
        collections::HashMap,
        net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc, RwLock,
        },
        thread::{self, spawn, JoinHandle, Result},
        time::{Duration, Instant, SystemTime},
    },
    tokio_util::sync::CancellationToken,
};

#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

const SINK_REPORT_INTERVAL: Duration = Duration::from_secs(5);
const SINK_RECEIVE_TIMEOUT: Duration = Duration::from_secs(1);
const SOCKET_RECEIVE_TIMEOUT: Duration = Duration::from_secs(1);

fn sink(
    exit: Arc<AtomicBool>,
    received_size: Arc<AtomicUsize>,
    receiver: PacketBatchReceiver,
    verbose: bool,
) -> JoinHandle<()> {
    spawn(move || {
        let mut last_report = Instant::now();
        let mut last_count = 0;
        while !exit.load(Ordering::Relaxed) {
            if let Ok(packet_batch) = receiver.recv_timeout(SINK_RECEIVE_TIMEOUT) {
                received_size.fetch_add(packet_batch.len(), Ordering::Relaxed);
            }

            let count = received_size.load(Ordering::Relaxed);

            if verbose && last_report.elapsed() > SINK_REPORT_INTERVAL {
                let change = count - last_count;
                let rate = change as u64 / SINK_REPORT_INTERVAL.as_secs();
                println!("Received txns count: total: {count}, rate {rate}/s");
                last_report = Instant::now();
                last_count = count;
            }
        }
    })
}

const TRANSACTIONS_PER_THREAD: u64 = 1_000_000; // Number of transactions per thread

fn main() -> Result<()> {
    let matches = App::new(crate_name!())
        .about(crate_description!())
        .version(solana_version::version!())
        .arg(
            Arg::with_name("identity")
                .short("i")
                .long("identity")
                .value_name("KEYPAIR")
                .takes_value(true)
                .validator(is_keypair_or_ask_keyword)
                .help(
                    "Identity keypair for the QUIC endpoint. If it is not specified a random key \
                     is created.",
                ),
        )
        .arg(
            Arg::with_name("num-recv-sockets")
                .long("num-recv-sockets")
                .value_name("NUM")
                .takes_value(true)
                .help("Use NUM receive sockets"),
        )
        .arg(
            Arg::with_name("num-producers")
                .long("num-producers")
                .value_name("NUM")
                .takes_value(true)
                .help("Use this many producer threads."),
        )
        .arg(
            Arg::with_name("max-connections")
                .long("max-connections")
                .value_name("NUM")
                .takes_value(true)
                .help("Maximum concurrent client connections allowed on the server side."),
        )
        .arg(
            Arg::with_name("max-connections-per-peer")
                .long("max-connections-per-peer")
                .value_name("NUM")
                .takes_value(true)
                .help("Maximum concurrent client connections per peer allowed on the server side."),
        )
        .arg(
            Arg::with_name("max-connections-per-ipaddr-per-min")
                .long("max-connections-per-ipaddr-per-min")
                .value_name("NUM")
                .takes_value(true)
                .help(
                    "Maximum client connections per ipaddr per minute allowed on the server side.",
                ),
        )
        .arg(
            Arg::with_name("connection-pool-size")
                .long("connection-pool-size")
                .value_name("NUM")
                .takes_value(true)
                .help("Maximum concurrent client connections on the client side."),
        )
        .arg(
            Arg::with_name("server-only")
                .long("server-only")
                .takes_value(false)
                .help("Run the bench tool as a server only."),
        )
        .arg(
            Arg::with_name("client-only")
                .long("client-only")
                .takes_value(false)
                .requires("server-address")
                .help("Run the bench tool as a client only."),
        )
        .arg(
            Arg::with_name("server-address")
                .short("n")
                .long("server-address")
                .value_name("HOST:PORT")
                .takes_value(true)
                .validator(|arg| solana_net_utils::is_host_port(arg.to_string()))
                .help(
                    "The destination streamer address to which the client will send transactions \
                     to",
                ),
        )
        .arg(
            Arg::with_name("use-connection-cache")
                .long("use-connection-cache")
                .takes_value(false)
                .help("Use this many producer threads."),
        )
        .arg(
            Arg::with_name("verbose")
                .long("verbose")
                .takes_value(false)
                .help("Show verbose messages."),
        )
        .arg(
            Arg::with_name("use-quic")
                .long("use-quic")
                .value_name("Boolean")
                .takes_value(true)
                .default_value("false")
                .help("Controls if to use QUIC for sending/receiving vote transactions."),
        )
        .get_matches();

    agave_logger::setup();

    let mut num_sockets = 1usize;
    if let Some(n) = matches.value_of("num-recv-sockets") {
        num_sockets = max(num_sockets, n.to_string().parse().expect("integer"));
    }

    let vote_use_quic = value_t_or_exit!(matches, "use-quic", bool);
    let num_producers: u64 = value_t!(matches, "num-producers", u64).unwrap_or(4);

    let max_connections: usize =
        value_t!(matches, "max-connections", usize).unwrap_or(DEFAULT_MAX_STAKED_CONNECTIONS);
    let max_connections_per_peer: usize = value_t!(matches, "max-connections-per-peer", usize)
        .unwrap_or(DEFAULT_MAX_QUIC_CONNECTIONS_PER_UNSTAKED_PEER);
    let max_connections_per_ipaddr_per_min: usize =
        value_t!(matches, "max-connections-per-ipaddr-per-min", usize).unwrap_or(1024); // Default value for max connections per ipaddr per minute
    let connection_pool_size: usize =
        value_t!(matches, "connection-pool-size", usize).unwrap_or(256);

    let use_connection_cache = matches.is_present("use-connection-cache");
    let server_only = matches.is_present("server-only");
    let client_only = matches.is_present("client-only");
    let verbose = matches.is_present("verbose");

    let destination = matches.is_present("server-address").then(|| {
        let addr = matches
            .value_of("server-address")
            .expect("Server address must be set when --client-only is used");
        solana_net_utils::parse_host_port(addr).expect("Expecting a valid server address")
    });

    let port = destination.map_or(0, |addr| addr.port());
    let ip_addr = destination.map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |addr| addr.ip());

    let quic_params = vote_use_quic.then(|| {
        let identity_keypair = keypair_of(&matches, "identity")
            .or_else(|| {
                println!("--identity is not specified, will generate a key dynamically.");
                Some(Keypair::new())
            })
            .unwrap();

        let stake: u64 = 1024;
        let total_stake: u64 = 1024;

        let stakes = HashMap::from([
            (identity_keypair.pubkey(), stake),
            (Pubkey::new_unique(), total_stake.saturating_sub(stake)),
        ]);
        let staked_nodes: Arc<RwLock<StakedNodes>> = Arc::new(RwLock::new(StakedNodes::new(
            Arc::new(stakes),
            HashMap::<Pubkey, u64>::default(), // overrides
        )));

        QuicParams {
            identity_keypair,
            staked_nodes,
            connection_pool_size,
        }
    });

    let (exit, cancel, read_threads, sink_threads, destination) = if !client_only {
        let exit = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();

        let mut read_channels = Vec::new();
        let mut read_threads = Vec::new();
        let recycler = PacketBatchRecycler::default();
        let config = SocketConfig::default();
        let (port, read_sockets) = multi_bind_in_range_with_config(
            ip_addr,
            (port, port + num_sockets as u16),
            config,
            num_sockets,
        )
        .unwrap();
        let stats = Arc::new(StreamerReceiveStats::new("bench-vote-test"));

        if let Some(quic_params) = &quic_params {
            let quic_server_params = QuicStreamerConfig {
                max_connections_per_ipaddr_per_min: max_connections_per_ipaddr_per_min
                    .try_into()
                    .unwrap(),
                ..Default::default()
            };
            let qos_config = SwQosConfig {
                max_connections_per_unstaked_peer: max_connections_per_peer,
                max_staked_connections: max_connections,
                max_unstaked_connections: 0,
                ..Default::default()
            };
            let (s_reader, r_reader) = unbounded();
            read_channels.push(r_reader);

            let server = spawn_stake_wighted_qos_server(
                "solRcvrBenVote",
                "bench_vote_metrics",
                read_sockets,
                &quic_params.identity_keypair,
                s_reader,
                quic_params.staked_nodes.clone(),
                quic_server_params,
                qos_config,
                cancel.clone(),
            )
            .unwrap();
            read_threads.push(server.thread);
        } else {
            for read in read_sockets {
                read.set_read_timeout(Some(SOCKET_RECEIVE_TIMEOUT)).unwrap();

                let (s_reader, r_reader) = unbounded();
                read_channels.push(r_reader);
                read_threads.push(receiver(
                    "solRcvrBenVote".to_string(),
                    Arc::new(read),
                    exit.clone(),
                    s_reader,
                    recycler.clone(),
                    stats.clone(),
                    None,  // coalesce
                    true,  // use_pinned_memory
                    None,  // in_vote_only_mode
                    false, // is_staked_service
                ));
            }
        }

        let received_size = Arc::new(AtomicUsize::new(0));
        let sink_threads: Vec<_> = read_channels
            .into_iter()
            .map(|r_reader| sink(exit.clone(), received_size.clone(), r_reader, verbose))
            .collect();

        let destination = SocketAddr::new(ip_addr, port);
        println!("Running server at {destination:?}");
        (
            Some(exit),
            Some(cancel),
            Some(read_threads),
            Some(sink_threads),
            destination,
        )
    } else {
        (None, None, None, None, destination.unwrap())
    };

    let start = SystemTime::now();

    let producer_threads = (!server_only).then(|| {
        producer(
            destination,
            num_producers,
            use_connection_cache,
            verbose,
            quic_params,
        )
    });

    producer_threads
        .into_iter()
        .flatten()
        .try_for_each(JoinHandle::join)?;

    if !server_only {
        if let Some(exit) = exit {
            exit.store(true, Ordering::Relaxed);
            cancel.unwrap().cancel();
        }
    } else {
        println!("To stop the server, please press ^C");
    }

    read_threads
        .into_iter()
        .flatten()
        .try_for_each(JoinHandle::join)?;
    sink_threads
        .into_iter()
        .flatten()
        .try_for_each(JoinHandle::join)?;

    if !(server_only) {
        let elapsed = start.elapsed().unwrap();
        let ftime = elapsed.as_nanos() as f64 / 1_000_000_000.0;
        let fcount = (TRANSACTIONS_PER_THREAD * num_producers) as f64;

        println!(
            "Performance: {:?}/s, count: {fcount}, time in second: {ftime}",
            fcount / ftime
        );
    }
    Ok(())
}

#[derive(Clone)]
enum Transporter {
    Cache(Arc<ConnectionCache>),
    DirectSocket(Arc<UdpSocket>),
}

struct QuicParams {
    identity_keypair: Keypair,
    staked_nodes: Arc<RwLock<StakedNodes>>,
    connection_pool_size: usize,
}

fn producer(
    sock: SocketAddr,
    num_producers: u64,
    use_connection_cache: bool,
    verbose: bool,
    quic_params: Option<QuicParams>,
) -> Vec<JoinHandle<()>> {
    println!("Running clients against {sock:?}");
    let transporter = if use_connection_cache || quic_params.is_some() {
        if let Some(quic_params) = &quic_params {
            Transporter::Cache(Arc::new(ConnectionCache::new_with_client_options(
                "connection_cache_vote_quic",
                quic_params.connection_pool_size,
                None, // client_endpoint
                Some((
                    &quic_params.identity_keypair,
                    IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
                )),
                Some((
                    &quic_params.staked_nodes,
                    &quic_params.identity_keypair.pubkey(),
                )),
            )))
        } else {
            Transporter::Cache(Arc::new(ConnectionCache::with_udp(
                "connection_cache_vote_udp",
                1, // connection_pool_size
            )))
        }
    } else {
        Transporter::DirectSocket(Arc::new(bind_to_unspecified().unwrap()))
    };

    let mut handles = vec![];

    let current_slot: u64 = 0;

    let identity_keypair = Keypair::new(); // Replace with loaded keypair

    for _i in 0..num_producers {
        let transporter = transporter.clone();
        let identity_keypair = identity_keypair.insecure_clone();
        handles.push(thread::spawn(move || {
            // Generate and send transactions
            for _j in 0..TRANSACTIONS_PER_THREAD {
                // Create a vote instruction
                let vote = Vote {
                    slots: vec![current_slot], // Voting for the current slot
                    hash: Hash::new_unique(),
                    timestamp: None, // Optional timestamp
                };

                let vote_instruction = vote_instruction::vote(
                    &identity_keypair.pubkey(),
                    &identity_keypair.pubkey(),
                    vote,
                );

                // Build the transaction
                let message = Message::new(&[vote_instruction], Some(&identity_keypair.pubkey()));

                let recent_blockhash = Hash::new_unique();
                let transaction = Transaction::new(&[&identity_keypair], message, recent_blockhash);

                let serialized_transaction = bincode::serialize(&transaction).unwrap();

                match &transporter {
                    Transporter::Cache(cache) => {
                        let connection = cache.get_connection(&sock);

                        match connection.send_data(&serialized_transaction) {
                            Ok(_) => {
                                if verbose {
                                    println!("Sent transaction successfully");
                                }
                            }
                            Err(ex) => {
                                println!("Error sending transaction {ex:?}");
                            }
                        }
                    }
                    Transporter::DirectSocket(socket) => {
                        match socket.send_to(&serialized_transaction, sock) {
                            Ok(_) => {
                                if verbose {
                                    println!(
                                        "Sent transaction via direct socket successfully {sock:?}"
                                    );
                                }
                            }
                            Err(ex) => {
                                println!("Error sending transaction {ex:?}");
                            }
                        }
                    }
                }
            }
        }));
    }
    handles
}
