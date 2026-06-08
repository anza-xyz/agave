#![allow(clippy::arithmetic_side_effects)]
//! Two-process variant of `vote_connection_cache` for running across
//! network namespaces with an emulated link.
//!
//! Same wire protocol and same ConnectionCache → SimpleQos QUIC server
//! shape as `vote_connection_cache`, but split into `--role server` and
//! `--role client` so each side can run in its own netns. Keypairs and
//! stake map are derived deterministically from `--seed` so the two
//! sides agree on identities without any out-of-band exchange. Send
//! timestamps are absolute `CLOCK_MONOTONIC` nanoseconds, which is a
//! kernel-wide clock shared across namespaces on the same host, so the
//! server can compute one-way latencies directly.
//!
//! Typical use:
//!   # netns + veth setup done by the operator; e.g.
//!   #   ip netns exec ns-srv tc qdisc add dev veth-srv root netem delay 5ms
//!
//!   ip netns exec ns-srv ./vote_connection_cache_netns \
//!       --role server --seed 42 --peers 100 \
//!       --bind-ip 10.0.0.2 --port-base 9000 \
//!       --slot-ms 200 --slots 50 --packets-per-slot 4 --warmup-slots 5
//!
//!   ip netns exec ns-cli ./vote_connection_cache_netns \
//!       --role client --seed 42 --peers 100 \
//!       --server-ip 10.0.0.2 --port-base 9000 \
//!       --slot-ms 200 --slots 50 --packets-per-slot 4 --warmup-slots 5

use {
    clap::{Parser, ValueEnum},
    crossbeam_channel::{Receiver, bounded},
    log::info,
    solana_client::connection_cache::ConnectionCache,
    solana_connection_cache::client_connection::ClientConnection,
    solana_keypair::{Keypair, keypair_from_seed},
    solana_perf::packet::PacketBatch,
    solana_signer::Signer,
    solana_streamer::{
        nonblocking::{
            quic::SpawnNonBlockingServerResult, simple_qos::SimpleQosConfig,
            testing_utilities::spawn_simple_qos_server,
        },
        quic::QuicStreamerConfig,
        quic_socket::QuicSocket,
        streamer::StakedNodes,
    },
    std::{
        collections::HashMap,
        net::{IpAddr, SocketAddr, UdpSocket},
        sync::{
            Arc, RwLock,
            atomic::{AtomicU64, Ordering},
        },
        thread,
        time::Duration,
    },
    tokio::task::JoinHandle,
    tokio_util::sync::CancellationToken,
};

/// Layout: [slot: u32][send_monotonic_nanos: u64][padding...]
const PACKET_SIZE: usize = 500;

/// Sentinel slot value for warmup-batch probes. The server drains
/// these without recording a sample so probe traffic doesn't pollute
/// stats.
const PROBE_SLOT: u32 = u32::MAX;

/// Domain tag for the client identity in seed derivation.
const ROLE_CLIENT: u8 = 1;
/// Domain tag for server identities in seed derivation.
const ROLE_SERVER: u8 = 2;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Role {
    Server,
    Client,
}

#[derive(Debug, Parser)]
#[command(about = "ConnectionCache fanout benchmark across two netns processes")]
struct Cli {
    /// Process role.
    #[arg(long, value_enum)]
    role: Role,

    /// Shared seed both sides use to derive matching keypairs / stakes.
    /// Both processes MUST be invoked with the same value.
    #[arg(long, default_value_t = 42)]
    seed: u64,

    /// Number of server peers. Server binds N sockets; client targets
    /// N addresses. Both sides must agree.
    #[arg(long, default_value_t = 20)]
    peers: usize,

    /// Server IP address (where the client sends; what the server
    /// binds when `--bind-ip` is omitted).
    #[arg(long)]
    server_ip: IpAddr,

    /// First UDP port for server sockets. Server binds
    /// `port_base..port_base+peers`.
    #[arg(long, default_value_t = 9000)]
    port_base: u16,

    /// [server only] IP to bind to. Defaults to `--server-ip`.
    #[arg(long)]
    bind_ip: Option<IpAddr>,

    /// Slot duration in milliseconds. Both sides must agree so the
    /// server can compute its expected packet count and auto-shutdown
    /// deadline.
    #[arg(long, default_value_t = 200)]
    slot_ms: u64,

    /// Number of measurement slots. Must match on both sides.
    #[arg(long, default_value_t = 50)]
    slots: usize,

    /// Packets the client sends to each server per slot. Must match.
    #[arg(long, default_value_t = 4)]
    packets_per_slot: usize,

    /// Warmup slots (not counted in stats). Must match.
    #[arg(long, default_value_t = 5)]
    warmup_slots: usize,

    /// Max streams per second per connection for SimpleQos.
    #[arg(long, default_value_t = 50)]
    max_streams_per_second: u64,

    /// [client only] Seconds to wait before starting the probe phase,
    /// to let the server come up.
    #[arg(long, default_value_t = 2)]
    startup_delay_secs: u64,

    /// Per-peer stake. Identical on both sides so the staked-nodes map
    /// agrees.
    #[arg(long, default_value_t = 1_000_000)]
    stake_per_peer: u64,
}

/// Absolute `CLOCK_MONOTONIC` nanoseconds. Shared across processes /
/// network namespaces on the same kernel, so the client's send
/// timestamp is directly comparable to the server's receive
/// timestamp.
fn monotonic_nanos() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid &mut to a libc::timespec and
    // CLOCK_MONOTONIC is always available on Linux.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    assert_eq!(rc, 0, "clock_gettime(CLOCK_MONOTONIC) failed");
    (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
}

fn encode_packet(slot: u32) -> Vec<u8> {
    let mut buf = vec![0u8; PACKET_SIZE];
    buf[0..4].copy_from_slice(&slot.to_le_bytes());
    let nanos = monotonic_nanos();
    buf[4..12].copy_from_slice(&nanos.to_le_bytes());
    buf
}

fn decode_packet(data: &[u8]) -> Option<(u32, u64)> {
    if data.len() < 12 {
        return None;
    }
    let slot = u32::from_le_bytes(data[0..4].try_into().ok()?);
    let send_nanos = u64::from_le_bytes(data[4..12].try_into().ok()?);
    Some((slot, send_nanos))
}

/// Build a 32-byte ed25519 secret deterministically from
/// `(seed, role, idx)`. Uniqueness across (role, idx) is sufficient
/// for bench identities — no cryptographic strength required.
fn derive_keypair(seed: u64, role: u8, idx: u32) -> Keypair {
    let mut bytes = [0u8; 32];
    bytes[0..8].copy_from_slice(&seed.to_le_bytes());
    bytes[8] = role;
    bytes[9..13].copy_from_slice(&idx.to_le_bytes());
    // Fill the tail with a non-trivial pattern that varies per role,
    // so the bytes used aren't mostly zero.
    for (i, b) in bytes.iter_mut().enumerate().skip(13) {
        *b = (i as u8)
            .wrapping_mul(role.wrapping_add(1))
            .wrapping_add(0x5a);
    }
    keypair_from_seed(&bytes).expect("32-byte seed is valid for ed25519")
}

fn build_keypairs(seed: u64, num_peers: usize) -> (Keypair, Vec<Keypair>) {
    let client = derive_keypair(seed, ROLE_CLIENT, 0);
    let servers = (0..num_peers)
        .map(|i| derive_keypair(seed, ROLE_SERVER, i as u32))
        .collect();
    (client, servers)
}

fn build_staked_nodes(
    client: &Keypair,
    servers: &[Keypair],
    stake_per_peer: u64,
) -> Arc<RwLock<StakedNodes>> {
    let mut stakes: HashMap<_, _> = servers
        .iter()
        .map(|kp| (kp.pubkey(), stake_per_peer))
        .collect();
    stakes.insert(client.pubkey(), stake_per_peer);
    Arc::new(RwLock::new(StakedNodes::new(
        Arc::new(stakes),
        HashMap::default(),
    )))
}

struct ServerSlot {
    handle: JoinHandle<()>,
    receiver: Receiver<PacketBatch>,
}

fn run_server(cli: Cli) -> anyhow::Result<()> {
    let num_peers = cli.peers;
    assert!(cli.packets_per_slot > 0, "packets_per_slot must be ≥ 1");

    let bind_ip = cli.bind_ip.unwrap_or(cli.server_ip);
    let warmup_bursts = cli.warmup_slots * cli.packets_per_slot;
    let total_bursts = (cli.warmup_slots + cli.slots) * cli.packets_per_slot;

    info!(
        "[server] seed={} peers={} bind={}:{}..{} slot_ms={} slots={} ppsl={} warmup_slots={}",
        cli.seed,
        num_peers,
        bind_ip,
        cli.port_base,
        cli.port_base as usize + num_peers - 1,
        cli.slot_ms,
        cli.slots,
        cli.packets_per_slot,
        cli.warmup_slots,
    );

    let (client_keypair, server_keypairs) = build_keypairs(cli.seed, num_peers);
    let staked_nodes = build_staked_nodes(&client_keypair, &server_keypairs, cli.stake_per_peer);

    let server_cancel = CancellationToken::new();

    let server_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(32)
        .enable_all()
        .build()?;
    let _server_guard = server_runtime.enter();

    info!("[server] spawning {num_peers} QUIC servers...");
    let mut servers: Vec<ServerSlot> = Vec::with_capacity(num_peers);
    for (idx, keypair) in server_keypairs.iter().enumerate() {
        let port = cli
            .port_base
            .checked_add(idx as u16)
            .expect("port_base + idx overflows u16");
        let server_socket = UdpSocket::bind(SocketAddr::new(bind_ip, port))?;
        let (sender, receiver) = bounded(4096);

        let (
            SpawnNonBlockingServerResult {
                endpoints: _,
                stats: _,
                thread: handle,
                max_concurrent_connections: _,
            },
            _banlist,
        ) = spawn_simple_qos_server(
            "conn_cache_netns_bench",
            [QuicSocket::Kernel(server_socket)],
            keypair,
            sender,
            staked_nodes.clone(),
            QuicStreamerConfig {
                max_connections_per_ipaddr_per_min: num_peers as u64 * 1000,
                ..QuicStreamerConfig::default_for_tests()
            },
            SimpleQosConfig {
                max_streams_per_second: cli.max_streams_per_second,
                max_staked_connections: num_peers + 100,
                max_connections_per_peer: 2,
            },
            server_cancel.clone(),
        )?;

        servers.push(ServerSlot { handle, receiver });

        if (idx + 1) % 100 == 0 {
            info!("[server]   spawned {}/{}", idx + 1, num_peers);
        }
    }
    info!("[server] all {num_peers} servers up");

    let received_count = Arc::new(AtomicU64::new(0));
    let latencies: Arc<std::sync::Mutex<Vec<f64>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

    // Drain threads: shard N receivers across K=4 OS threads with
    // `crossbeam_channel::Select`. Same pattern as the in-process bench.
    const DRAIN_THREADS: usize = 4;
    let rxs: Vec<Receiver<PacketBatch>> = servers.iter().map(|s| s.receiver.clone()).collect();
    let shard_size = rxs.len().div_ceil(DRAIN_THREADS).max(1);
    let warmup_threshold = warmup_bursts as u32;
    let receiver_handles: Vec<thread::JoinHandle<()>> = rxs
        .chunks(shard_size)
        .enumerate()
        .map(|(shard_idx, chunk)| {
            let shard: Vec<Receiver<PacketBatch>> = chunk.to_vec();
            let latencies = latencies.clone();
            let received_count = received_count.clone();
            thread::Builder::new()
                .name(format!("bench-drain-{shard_idx}"))
                .spawn(move || {
                    let mut sel = crossbeam_channel::Select::new();
                    for rx in &shard {
                        sel.recv(rx);
                    }
                    let mut local_latencies: Vec<f64> = Vec::new();
                    let process = |batch: &PacketBatch, local: &mut Vec<f64>| {
                        let recv_nanos = monotonic_nanos();
                        for pkt in batch.iter() {
                            let pkt = pkt.to_bytes_packet();
                            if let Some((slot, send_nanos)) = decode_packet(pkt.buffer()) {
                                if slot == PROBE_SLOT {
                                    continue;
                                }
                                received_count.fetch_add(1, Ordering::Relaxed);
                                if slot >= warmup_threshold {
                                    let latency_us =
                                        recv_nanos.saturating_sub(send_nanos) as f64 / 1000.0;
                                    local.push(latency_us);
                                }
                            }
                        }
                    };
                    loop {
                        let oper = sel.select();
                        let idx = oper.index();
                        match oper.recv(&shard[idx]) {
                            Ok(batch) => process(&batch, &mut local_latencies),
                            Err(_) => {
                                for rx in &shard {
                                    while let Ok(batch) = rx.try_recv() {
                                        process(&batch, &mut local_latencies);
                                    }
                                }
                                break;
                            }
                        }
                    }
                    latencies
                        .lock()
                        .expect("lock")
                        .extend_from_slice(&local_latencies);
                })
                .expect("spawn drain thread")
        })
        .collect();

    // Run until the client should be done plus slack: probe + bursts
    // (warmup+measured) + tail. The server has no signal from the
    // client, so we use a wall-clock budget derived from shared CLI.
    let slot_duration = Duration::from_millis(cli.slot_ms);
    let burst_interval = slot_duration / cli.packets_per_slot as u32;
    let total_run_secs = ((total_bursts as u32 * burst_interval).as_secs_f64()
        + cli.startup_delay_secs as f64
        + 8.0) as u64;
    info!("[server] running for ~{total_run_secs}s then shutting down");
    thread::sleep(Duration::from_secs(total_run_secs));

    // Shut down servers and join drain threads.
    info!("[server] cancelling servers...");
    server_cancel.cancel();
    for server in servers.drain(..) {
        drop(server.receiver);
        server_runtime.block_on(async {
            let _ = server.handle.await;
        });
    }
    for h in receiver_handles {
        let _ = h.join();
    }

    let total_received = received_count.load(Ordering::Relaxed);
    let measured_expected = (cli.slots * num_peers * cli.packets_per_slot) as u64;
    let total_expected = (total_bursts * num_peers) as u64;

    info!(
        "[server] total packets: expected={total_expected} received={total_received} \
         delivery_rate={:.2}%",
        (total_received as f64 / total_expected as f64) * 100.0,
    );

    let latencies = latencies.lock().expect("lock");
    if latencies.is_empty() {
        println!("\nWARNING: No latency samples collected from measured slots!");
        return Ok(());
    }
    let n = latencies.len();
    let sum: f64 = latencies.iter().sum();
    let mean = sum / n as f64;
    let variance: f64 = latencies
        .iter()
        .map(|v| (v - mean) * (v - mean))
        .sum::<f64>()
        / n as f64;
    let stddev = variance.sqrt();
    let mut sorted = latencies.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = sorted[n / 2];
    let p99 = sorted[(n as f64 * 0.99) as usize];
    let max = sorted[n - 1];
    let min = sorted[0];

    println!("\n=== Delivery Statistics (measured slots only) ===");
    println!("  Packets expected:  {measured_expected}");
    println!("  Packets received:  {n}");
    println!(
        "  Delivery rate:     {:.2}%",
        (n as f64 / measured_expected as f64) * 100.0,
    );
    println!("\n=== Latency Statistics (microseconds, one-way) ===");
    println!("  Mean:    {mean:.1} us");
    println!("  Stddev:  {stddev:.1} us");
    println!("  Min:     {min:.1} us");
    println!("  P50:     {p50:.1} us");
    println!("  P99:     {p99:.1} us");
    println!("  Max:     {max:.1} us");

    Ok(())
}

fn run_client(cli: Cli) -> anyhow::Result<()> {
    let num_peers = cli.peers;
    assert!(cli.packets_per_slot > 0, "packets_per_slot must be ≥ 1");

    let slot_duration = Duration::from_millis(cli.slot_ms);
    let burst_interval = slot_duration / cli.packets_per_slot as u32;
    let warmup_bursts = cli.warmup_slots * cli.packets_per_slot;
    let total_bursts = (cli.warmup_slots + cli.slots) * cli.packets_per_slot;

    info!(
        "[client] seed={} peers={} target={}:{}..{} slot_ms={} slots={} ppsl={} warmup_slots={} \
         burst_interval={:.1}ms total_bursts={}",
        cli.seed,
        num_peers,
        cli.server_ip,
        cli.port_base,
        cli.port_base as usize + num_peers - 1,
        cli.slot_ms,
        cli.slots,
        cli.packets_per_slot,
        cli.warmup_slots,
        burst_interval.as_secs_f64() * 1000.0,
        total_bursts,
    );

    let (client_keypair, server_keypairs) = build_keypairs(cli.seed, num_peers);
    let staked_nodes = build_staked_nodes(&client_keypair, &server_keypairs, cli.stake_per_peer);

    info!(
        "[client] waiting {}s for server to come up...",
        cli.startup_delay_secs
    );
    thread::sleep(Duration::from_secs(cli.startup_delay_secs));

    // Use port 0 on the wildcard so the client's egress IP is chosen
    // by routing (i.e. the namespace's veth address). This matches how
    // a real validator binds.
    let client_socket = UdpSocket::bind(SocketAddr::new(
        IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
        0,
    ))?;
    let local_ip = match cli.server_ip {
        IpAddr::V4(_) => IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
        IpAddr::V6(_) => IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
    };
    let connection_cache = Arc::new(ConnectionCache::new_with_client_options(
        "BenchAlpenglowConnectionCacheNetns",
        1,
        Some(client_socket),
        Some((&client_keypair, local_ip)),
        Some((&staked_nodes, &client_keypair.pubkey())),
    ));

    let all_addrs: Vec<SocketAddr> = (0..num_peers)
        .map(|i| SocketAddr::new(cli.server_ip, cli.port_base + i as u16))
        .collect();

    // Probe / connection establishment, in slices of 128.
    info!("[client] establishing connections to {num_peers} servers...");
    {
        const BATCH_SIZE: usize = 128;
        const BATCH_PAUSE: Duration = Duration::from_millis(100);
        let num_batches = num_peers.div_ceil(BATCH_SIZE);
        for (i, chunk) in all_addrs.chunks(BATCH_SIZE).enumerate() {
            let buf = encode_packet(PROBE_SLOT);
            for addr in chunk {
                let conn = connection_cache.get_connection(addr);
                let _ = conn.send_data_async(Arc::new(buf.clone()));
            }
            info!(
                "[client]   batch {}/{}: {} connections",
                i + 1,
                num_batches,
                chunk.len()
            );
            if i + 1 < num_batches {
                thread::sleep(BATCH_PAUSE);
            }
        }
        thread::sleep(Duration::from_secs(2));
        info!("[client] connection establishment complete");
    }

    info!(
        "[client] starting {} bursts ({} warmup + {} measured)...",
        total_bursts,
        warmup_bursts,
        cli.slots * cli.packets_per_slot,
    );

    for burst in 0..total_bursts as u32 {
        let burst_start = std::time::Instant::now();
        let is_warmup = (burst as usize) < warmup_bursts;

        let buf = encode_packet(burst);
        for addr in &all_addrs {
            let conn = connection_cache.get_connection(addr);
            let _ = conn.send_data_async(Arc::new(buf.clone()));
        }

        let send_elapsed = burst_start.elapsed();
        if send_elapsed < burst_interval {
            thread::sleep(burst_interval - send_elapsed);
        }

        info!(
            "[client] burst {}{}: send phase {:.1}ms, expected {} packets",
            burst,
            if is_warmup { " (warmup)" } else { "" },
            send_elapsed.as_secs_f64() * 1000.0,
            num_peers,
        );
    }

    info!("[client] flushing in-flight packets...");
    thread::sleep(Duration::from_secs(3));

    info!("[client] dropping connection cache and exiting");
    drop(connection_cache);
    Ok(())
}

fn main() -> anyhow::Result<()> {
    agave_logger::setup();
    let cli = Cli::parse();
    match cli.role {
        Role::Server => run_server(cli),
        Role::Client => run_client(cli),
    }
}
