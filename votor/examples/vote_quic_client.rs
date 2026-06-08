#![allow(clippy::arithmetic_side_effects)]
//! QUIC fanout benchmark for VotorQuicClient.
//!
//! Two topologies, selected by `--direction`:
//!   - `egress`  (default): 1 client → N servers (the original bench).
//!   - `ingress`: N clients → 1 server. Each client is a separate
//!     `VotorQuicClient` instance on a single shared 32-thread runtime; the
//!     single server runs on a 4-thread runtime — exact mirror of the egress
//!     thread split.
//!
//! Per-slot send rate is `packets_per_slot` per client (egress: 1 client ×
//! N peers × p/slot; ingress: N clients × 1 peer × p/slot). Fan-out volume
//! is therefore the same on both sides.
//!
//! Usage:
//!   cargo run --example vote_quic_client -p agave-votor -- --peers 100 --slot-ms 200 --slots 5
//!   cargo run --example vote_quic_client -p agave-votor -- --direction ingress --peers 100

use {
    agave_votor::quic_client::VotorQuicClient,
    clap::{Parser, ValueEnum},
    crossbeam_channel::{Receiver, bounded},
    log::info,
    solana_keypair::Keypair,
    solana_net_utils::sockets::bind_to_localhost_unique,
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
    solana_tpu_client_next::connection_workers_scheduler::{BindTarget, StakeIdentity},
    std::{
        collections::HashMap,
        net::SocketAddr,
        sync::{
            Arc, RwLock,
            atomic::{AtomicU64, Ordering},
        },
        thread,
        time::{Duration, Instant},
    },
    tokio::task::JoinHandle,
    tokio_util::sync::CancellationToken,
};

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Direction {
    /// 1 client → N servers (egress fan-out).
    Egress,
    /// N clients → 1 server (ingress fan-in).
    Ingress,
}

#[derive(Debug, Parser)]
#[command(about = "VotorQuicClient fanout benchmark (egress: 1→N, ingress: N→1)")]
struct Cli {
    /// Number of peers. Servers in egress mode; clients in ingress mode.
    #[arg(long, default_value_t = 20)]
    peers: usize,

    /// Slot duration in milliseconds
    #[arg(long, default_value_t = 200)]
    slot_ms: u64,

    /// Number of slots to run
    #[arg(long, default_value_t = 50)]
    slots: usize,

    /// Packets sent per slot per source. Egress: per client → each server.
    /// Ingress: per client → the server.
    #[arg(long, default_value_t = 4)]
    packets_per_slot: usize,

    /// Warmup slots (not counted in stats)
    #[arg(long, default_value_t = 5)]
    warmup_slots: usize,

    /// Max streams per second per connection for SimpleQos
    #[arg(long, default_value_t = 50)]
    max_streams_per_second: u64,

    /// Test direction (egress = 1→N, ingress = N→1).
    #[arg(long, value_enum, default_value_t = Direction::Egress)]
    direction: Direction,
}

/// Layout: [slot: u32][send_time_nanos: u64][padding...]
const PACKET_SIZE: usize = 500;

/// Sentinel slot value for warmup-batch probes. Drain threads recognise
/// this and skip the probe entirely (no `received_count` bump, no
/// latency sample) so the post-run delivery rate is computed against
/// real measured traffic only.
const PROBE_SLOT: u32 = u32::MAX;

fn encode_packet(slot: u32, epoch: Instant) -> Vec<u8> {
    let mut buf = vec![0u8; PACKET_SIZE];
    buf[0..4].copy_from_slice(&slot.to_le_bytes());
    let nanos = epoch.elapsed().as_nanos() as u64;
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

struct Server {
    handle: JoinHandle<()>,
    receiver: Receiver<PacketBatch>,
    addr: SocketAddr,
}

fn print_stats(slots: usize, num_peers: usize, packets_per_slot: usize, latencies: &[f64]) {
    if latencies.is_empty() {
        println!("\nWARNING: No latency samples collected from measured slots!");
        return;
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
    let mut sorted = latencies.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = sorted[n / 2];
    let p99 = sorted[(n as f64 * 0.99) as usize];
    let max = sorted[n - 1];
    let min = sorted[0];
    let measured_expected = (slots * num_peers * packets_per_slot) as u64;

    println!("\n=== Delivery Statistics (measured slots only) ===");
    println!("  Packets expected:  {measured_expected}");
    println!("  Packets received:  {n}");
    let delivery_rate = (n as f64 / measured_expected as f64) * 100.0;
    println!("  Delivery rate:     {delivery_rate:.2}%");
    println!("\n=== Latency Statistics (microseconds) ===");
    println!("  Mean:    {mean:.1} us");
    println!("  Stddev:  {stddev:.1} us");
    println!("  Min:     {min:.1} us");
    println!("  P50:     {p50:.1} us");
    println!("  P99:     {p99:.1} us");
    println!("  Max:     {max:.1} us");
}

fn main() -> anyhow::Result<()> {
    agave_logger::setup();
    let cli = Cli::parse();
    match cli.direction {
        Direction::Egress => run_egress(cli),
        Direction::Ingress => run_ingress(cli),
    }
}

fn run_egress(cli: Cli) -> anyhow::Result<()> {
    let num_peers = cli.peers;
    assert!(cli.packets_per_slot > 0, "packets_per_slot must be ≥ 1");
    let slot_duration = Duration::from_millis(cli.slot_ms);
    let burst_interval = slot_duration / cli.packets_per_slot as u32;
    let warmup_bursts = cli.warmup_slots * cli.packets_per_slot;
    let total_bursts = (cli.warmup_slots + cli.slots) * cli.packets_per_slot;

    info!(
        "Starting EGRESS fanout benchmark: 1 client → {} servers, {}ms slots, {} measurement \
         slots, {} packets/slot/server (burst_interval={:.1}ms, total_bursts={})",
        num_peers,
        cli.slot_ms,
        cli.slots,
        cli.packets_per_slot,
        burst_interval.as_secs_f64() * 1000.0,
        total_bursts,
    );

    // Generate keypairs: one for the client, one per server
    let client_keypair = Keypair::new();
    let server_keypairs: Vec<Keypair> = (0..num_peers).map(|_| Keypair::new()).collect();

    // Build the stake map: client + all servers are staked
    let mut stakes: HashMap<_, _> = server_keypairs
        .iter()
        .map(|kp| (kp.pubkey(), 1_000_000u64))
        .collect();
    stakes.insert(client_keypair.pubkey(), 1_000_000u64);
    let staked_nodes = Arc::new(RwLock::new(StakedNodes::new(
        Arc::new(stakes),
        HashMap::default(),
    )));

    let server_cancel = CancellationToken::new();
    let client_cancel = CancellationToken::new();

    // Server runtime: 32 threads for all QUIC servers.
    let server_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(32)
        .enable_all()
        .build()?;
    let _server_guard = server_runtime.enter();

    // Client runtime: same config as TVU (4 threads via spawn_runtime).
    let client_runtime = VotorQuicClient::spawn_runtime()?;

    // Spawn servers
    info!("Spawning {num_peers} servers...");
    let spawn_start = Instant::now();
    let mut servers: Vec<Server> = Vec::with_capacity(num_peers);

    for (idx, keypair) in server_keypairs.iter().enumerate() {
        let server_socket = bind_to_localhost_unique()?;
        let server_addr = server_socket.local_addr()?;
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
            "all2all_bench",
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

        servers.push(Server {
            handle,
            receiver,
            addr: server_addr,
        });

        if (idx + 1) % 100 == 0 {
            info!("  spawned {}/{} servers", idx + 1, num_peers);
        }
    }

    // Create the single client on its own runtime (matches TVU: 4 threads).
    let client_socket = bind_to_localhost_unique()?;
    let (mut quic_client, _update_handler) = VotorQuicClient::new(
        client_runtime.handle().clone(),
        BindTarget::Socket(client_socket),
        StakeIdentity::new(&client_keypair),
        client_cancel.clone(),
    )?;

    let spawn_elapsed = spawn_start.elapsed();
    info!(
        "1 client + {} servers spawned in {:.2}s",
        num_peers,
        spawn_elapsed.as_secs_f64()
    );

    // Let servers settle
    thread::sleep(Duration::from_secs(1));

    let all_addrs: Vec<SocketAddr> = servers.iter().map(|s| s.addr).collect();

    // Shared timing epoch
    let epoch = Instant::now();

    // Received packet counter
    let received_count = Arc::new(AtomicU64::new(0));

    // Latency collection
    let latencies: Arc<std::sync::Mutex<Vec<f64>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

    // Drain threads: shard all N server receivers across K=4 OS threads,
    // each owning a contiguous chunk and blocking via
    // `crossbeam_channel::Select`. Replaces the prior "one thread per
    // server" pattern that, at N=2000, kept 2000 threads parked and
    // produced heavy syscall churn on the producer's `try_send` path
    // (each parked recv has its own futex; producer wakes one per send).
    // K=4 keeps the per-call `Select` scan bounded to ~N/4 handles so
    // the receive side isn't serialized while drastically reducing
    // wakeup overhead.
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
                        let recv_nanos = epoch.elapsed().as_nanos() as u64;
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
                                // First disconnect = shutdown started.
                                // Drain remaining batches from every
                                // receiver in this shard and exit; no
                                // more deliveries are coming.
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

    // Establish connections in slices of 128 servers, 100ms apart.
    info!("Establishing connections to {num_peers} servers...");
    {
        const BATCH_SIZE: usize = 128;
        const BATCH_PAUSE: Duration = Duration::from_millis(100);
        let num_batches = num_peers.div_ceil(BATCH_SIZE);
        for (i, chunk) in all_addrs.chunks(BATCH_SIZE).enumerate() {
            let buf = encode_packet(PROBE_SLOT, epoch);
            quic_client.send_message_to_peers(buf, chunk.iter().copied());
            info!(
                "  batch {}/{}: {} connections",
                i + 1,
                num_batches,
                chunk.len()
            );
            if i + 1 < num_batches {
                thread::sleep(BATCH_PAUSE);
            }
        }
        // Wait for last batch of handshakes to complete (1s timeout + margin).
        thread::sleep(Duration::from_secs(2));
        info!("Connection establishment complete.");
    }

    info!(
        "Starting {} bursts ({} warmup + {} measured)...",
        total_bursts,
        warmup_bursts,
        cli.slots * cli.packets_per_slot,
    );

    for burst in 0..total_bursts as u32 {
        let burst_start = Instant::now();
        let is_warmup = (burst as usize) < warmup_bursts;

        let buf = encode_packet(burst, epoch);
        quic_client.send_message_to_peers(buf, all_addrs.iter().copied());

        let send_elapsed = burst_start.elapsed();
        if send_elapsed < burst_interval {
            thread::sleep(burst_interval - send_elapsed);
        }

        info!(
            "Burst {}{}: send phase {:.1}ms, expected {} packets, total received so far: {}",
            burst,
            if is_warmup { " (warmup)" } else { "" },
            send_elapsed.as_secs_f64() * 1000.0,
            num_peers,
            received_count.load(Ordering::Relaxed),
        );
    }

    // Allow extra time for in-flight packets to arrive
    info!("Waiting for in-flight packets...");
    thread::sleep(Duration::from_secs(3));

    let total_received = received_count.load(Ordering::Relaxed);
    let total_expected = (total_bursts * num_peers) as u64;

    info!(
        "Total packets: expected={}, received={}, delivery_rate={:.2}%",
        total_expected,
        total_received,
        (total_received as f64 / total_expected as f64) * 100.0,
    );

    // Shutdown: gracefully close client connections, then tear down servers.
    info!("Shutting down client...");
    quic_client.shutdown();
    drop(quic_client);
    client_cancel.cancel();

    info!("Shutting down servers...");
    server_cancel.cancel();
    for server in servers.drain(..) {
        drop(server.receiver);
        server_runtime.block_on(async {
            let _ = server.handle.await;
        });
    }

    // Wait for receiver threads
    for h in receiver_handles {
        let _ = h.join();
    }

    let latencies = latencies.lock().expect("lock");
    print_stats(cli.slots, num_peers, cli.packets_per_slot, &latencies);

    info!("Done.");
    Ok(())
}

fn run_ingress(cli: Cli) -> anyhow::Result<()> {
    let num_peers = cli.peers; // here = number of clients
    assert!(cli.packets_per_slot > 0, "packets_per_slot must be ≥ 1");
    let slot_duration = Duration::from_millis(cli.slot_ms);
    let burst_interval = slot_duration / cli.packets_per_slot as u32;
    let warmup_bursts = cli.warmup_slots * cli.packets_per_slot;
    let total_bursts = (cli.warmup_slots + cli.slots) * cli.packets_per_slot;

    info!(
        "Starting INGRESS fanin benchmark: {} clients → 1 server, {}ms slots, {} measurement \
         slots, {} packets/slot/client (burst_interval={:.1}ms, total_bursts={})",
        num_peers,
        cli.slot_ms,
        cli.slots,
        cli.packets_per_slot,
        burst_interval.as_secs_f64() * 1000.0,
        total_bursts,
    );

    let server_keypair = Keypair::new();
    let client_keypairs: Vec<Keypair> = (0..num_peers).map(|_| Keypair::new()).collect();

    // Server-side stake map: every client + the server are staked. The
    // client side never sees a stake map — each VotorQuicClient is given
    // only the server addr to send to, so it cannot perceive the other
    // clients as peers.
    let mut stakes: HashMap<_, _> = client_keypairs
        .iter()
        .map(|kp| (kp.pubkey(), 1_000_000u64))
        .collect();
    stakes.insert(server_keypair.pubkey(), 1_000_000u64);
    let staked_nodes = Arc::new(RwLock::new(StakedNodes::new(
        Arc::new(stakes),
        HashMap::default(),
    )));

    let server_cancel = CancellationToken::new();
    let client_cancel = CancellationToken::new();

    // Flip of egress: small server runtime, larger shared client runtime.
    let server_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .thread_name("votor-bench-server")
        .build()?;
    let _server_guard = server_runtime.enter();

    let client_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(32)
        .enable_all()
        .thread_name("votor-bench-client")
        .build()?;

    info!("Spawning 1 server + {num_peers} clients...");
    let spawn_start = Instant::now();

    // 1 SimpleQos server. `bounded(1 << 17)` ≈ 130k batches buffer — at
    // N=2000 × 4 pkt/slot × 5 slot/s = 40k pkt/s this is ~3 s headroom if
    // the drain stalls.
    let server_socket = bind_to_localhost_unique()?;
    let server_addr = server_socket.local_addr()?;
    let (sender, server_rx) = bounded(1 << 17);

    let (
        SpawnNonBlockingServerResult {
            endpoints: _,
            stats: _,
            thread: server_handle,
            max_concurrent_connections: _,
        },
        _banlist,
    ) = spawn_simple_qos_server(
        "votor_ingress_bench",
        [QuicSocket::Kernel(server_socket)],
        &server_keypair,
        sender,
        staked_nodes.clone(),
        QuicStreamerConfig {
            // Every client lives on 127.0.0.1, so this cap is N-shaped.
            max_connections_per_ipaddr_per_min: (num_peers as u64) * 100,
            ..QuicStreamerConfig::default_for_tests()
        },
        SimpleQosConfig {
            max_streams_per_second: cli.max_streams_per_second,
            max_staked_connections: num_peers + 100,
            max_connections_per_peer: 1,
        },
        server_cancel.clone(),
    )?;

    // N clients, all on the SAME runtime handle (never call spawn_runtime
    // per client — that would create N × 4 worker threads).
    let mut clients: Vec<VotorQuicClient> = Vec::with_capacity(num_peers);
    for (idx, kp) in client_keypairs.iter().enumerate() {
        let sock = bind_to_localhost_unique()?;
        let (c, _upd) = VotorQuicClient::new(
            client_runtime.handle().clone(),
            BindTarget::Socket(sock),
            StakeIdentity::new(kp),
            client_cancel.clone(),
        )?;
        clients.push(c);
        if (idx + 1) % 100 == 0 {
            info!("  spawned {}/{} clients", idx + 1, num_peers);
        }
    }

    info!(
        "1 server + {} clients spawned in {:.2}s",
        num_peers,
        spawn_start.elapsed().as_secs_f64()
    );

    thread::sleep(Duration::from_secs(1));

    let epoch = Instant::now();
    let received_count = Arc::new(AtomicU64::new(0));
    let latencies: Arc<std::sync::Mutex<Vec<f64>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

    // Single drain thread on the one server rx — no Select needed.
    let warmup_threshold = warmup_bursts as u32;
    let drain_rx = server_rx.clone();
    let drain_received = received_count.clone();
    let drain_latencies = latencies.clone();
    let drain_handle = thread::Builder::new()
        .name("bench-drain".to_string())
        .spawn(move || {
            let process = |batch: &PacketBatch, local: &mut Vec<f64>| {
                let recv_nanos = epoch.elapsed().as_nanos() as u64;
                for pkt in batch.iter() {
                    let pkt = pkt.to_bytes_packet();
                    if let Some((slot, send_nanos)) = decode_packet(pkt.buffer()) {
                        if slot == PROBE_SLOT {
                            continue;
                        }
                        drain_received.fetch_add(1, Ordering::Relaxed);
                        if slot >= warmup_threshold {
                            let us = recv_nanos.saturating_sub(send_nanos) as f64 / 1000.0;
                            local.push(us);
                        }
                    }
                }
            };
            let mut local_latencies: Vec<f64> = Vec::new();
            while let Ok(batch) = drain_rx.recv() {
                process(&batch, &mut local_latencies);
            }
            while let Ok(batch) = drain_rx.try_recv() {
                process(&batch, &mut local_latencies);
            }
            drain_latencies
                .lock()
                .expect("lock")
                .extend_from_slice(&local_latencies);
        })
        .expect("spawn drain thread");

    // Warmup: each client sends one PROBE_SLOT to server. Batch by
    // clients (not addrs as in egress) — 128 clients per batch, 100 ms
    // pause between batches.
    info!("Establishing connections from {num_peers} clients to server...");
    {
        const BATCH_SIZE: usize = 128;
        const BATCH_PAUSE: Duration = Duration::from_millis(100);
        let num_batches = num_peers.div_ceil(BATCH_SIZE);
        for (i, chunk) in clients.chunks_mut(BATCH_SIZE).enumerate() {
            let buf = encode_packet(PROBE_SLOT, epoch);
            for c in chunk.iter_mut() {
                c.send_message_to_peers(buf.clone(), std::iter::once(server_addr));
            }
            info!(
                "  batch {}/{}: {} connections",
                i + 1,
                num_batches,
                chunk.len()
            );
            if i + 1 < num_batches {
                thread::sleep(BATCH_PAUSE);
            }
        }
        thread::sleep(Duration::from_secs(2));
        info!("Connection establishment complete.");
    }

    info!(
        "Starting {} bursts ({} warmup + {} measured)...",
        total_bursts,
        warmup_bursts,
        cli.slots * cli.packets_per_slot,
    );

    for burst in 0..total_bursts as u32 {
        let burst_start = Instant::now();
        let is_warmup = (burst as usize) < warmup_bursts;

        let buf = encode_packet(burst, epoch);
        for c in clients.iter_mut() {
            c.send_message_to_peers(buf.clone(), std::iter::once(server_addr));
        }

        let send_elapsed = burst_start.elapsed();
        if send_elapsed < burst_interval {
            thread::sleep(burst_interval - send_elapsed);
        }

        info!(
            "Burst {}{}: send phase {:.1}ms, expected {} packets, total received so far: {}",
            burst,
            if is_warmup { " (warmup)" } else { "" },
            send_elapsed.as_secs_f64() * 1000.0,
            num_peers,
            received_count.load(Ordering::Relaxed),
        );
    }

    info!("Waiting for in-flight packets...");
    thread::sleep(Duration::from_secs(3));

    let total_received = received_count.load(Ordering::Relaxed);
    let total_expected = (total_bursts * num_peers) as u64;
    info!(
        "Total packets: expected={}, received={}, delivery_rate={:.2}%",
        total_expected,
        total_received,
        (total_received as f64 / total_expected as f64) * 100.0,
    );

    // Shutdown: drain clients gracefully → cancel server → join.
    info!("Shutting down {} clients...", clients.len());
    for c in clients.iter_mut() {
        c.shutdown();
    }
    drop(clients);
    client_cancel.cancel();

    info!("Shutting down server...");
    server_cancel.cancel();
    drop(server_rx);
    server_runtime.block_on(async {
        let _ = server_handle.await;
    });

    let _ = drain_handle.join();

    let latencies = latencies.lock().expect("lock");
    print_stats(cli.slots, num_peers, cli.packets_per_slot, &latencies);

    info!("Done.");
    Ok(())
}
