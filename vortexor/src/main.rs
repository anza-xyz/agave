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
        sender::{
            PacketBatchSender, DEFAULT_BATCH_SIZE, DEFAULT_RECV_TIMEOUT,
            DEFAULT_SENDER_THREADS_COUNT,
        },
        vortexor::Vortexor,
    },
    std::{
        env,
        net::IpAddr,
        sync::{atomic::AtomicBool, Arc, RwLock},
        thread::JoinHandle,
        time::Duration,
    },
};

#[cfg(unix)]
fn redirect_stderr(filename: &str) {
    use std::{fs::OpenOptions, os::unix::io::AsRawFd};
    match OpenOptions::new().create(true).append(true).open(filename) {
        Ok(file) => unsafe {
            libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO);
        },
        Err(err) => eprintln!("Unable to open {filename}: {err}"),
    }
}

// Redirect stderr to a file with support for logrotate by sending a SIGUSR1 to the process.
//
// Upon success, future `log` macros and `eprintln!()` can be found in the specified log file.
pub fn redirect_stderr_to_file(logfile: Option<String>) -> Option<JoinHandle<()>> {
    // Default to RUST_BACKTRACE=1 for more informative validator logs
    if env::var_os("RUST_BACKTRACE").is_none() {
        env::set_var("RUST_BACKTRACE", "1")
    }

    match logfile {
        None => {
            solana_logger::setup_with_default_filter();
            None
        }
        Some(logfile) => {
            #[cfg(unix)]
            {
                use log::info;
                let mut signals =
                    signal_hook::iterator::Signals::new([signal_hook::consts::SIGUSR1])
                        .unwrap_or_else(|err| {
                            eprintln!("Unable to register SIGUSR1 handler: {err:?}");
                            exit(1);
                        });

                solana_logger::setup_with_default_filter();
                redirect_stderr(&logfile);
                Some(
                    std::thread::Builder::new()
                        .name("solSigUsr1".into())
                        .spawn(move || {
                            for signal in signals.forever() {
                                info!(
                                    "received SIGUSR1 ({}), reopening log file: {:?}",
                                    signal, logfile
                                );
                                redirect_stderr(&logfile);
                            }
                        })
                        .unwrap(),
                )
            }
            #[cfg(not(unix))]
            {
                println!("logrotate is not supported on this platform");
                solana_logger::setup_file_with_default(&logfile, solana_logger::DEFAULT_FILTER);
                None
            }
        }
    }
}

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
        bind_in_range_with_config(*bind_address, *dynamic_port_range, config).unwrap();

    // The non_vote_receiver will forward the verified transactions to its configured validator
    let (non_vote_sender, non_vote_receiver) = banking_tracer.create_channel_non_vote();

    let destinations = args.destination;

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
        *max_connections_per_peer,
        *max_tpu_staked_connections,
        *max_tpu_unstaked_connections,
        *max_fwd_staked_connections,
        *max_fwd_unstaked_connections,
        *max_streams_per_ms,
        *max_connections_per_ipaddr_per_min,
        tpu_coalesce,
        &identity_keypair,
        exit,
    );
    vortexor.join().unwrap();
    sigverify_stage.join().unwrap();
    packet_sender.join().unwrap();
}
