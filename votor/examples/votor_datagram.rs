#![allow(clippy::arithmetic_side_effects, clippy::manual_clamp)]
//! Fan-out / fan-in benchmark exercising [`agave_votor::datagram_endpoint`].
//!
//! Two topologies, selected by `--direction`:
//!   - `egress`  (default): 1 client → N servers. Per-slot fan-out from
//!     one sender to many receivers; matches production
//!     `VotingService::broadcast_consensus_message`. Apples-to-apples
//!     against bench A (`vote_quic_client`, tpu-client-next) and bench B
//!     (`vote_connection_cache`, ConnectionCache) on the
//!     `alpenglow-tpu-client-next` branch.
//!   - `ingress`: N clients → 1 server. Per-slot fan-in from many senders
//!     to one receiver, exercising the server-side accept / drain path.
//!     Thread split flipped vs egress: 4-thread server runtime, 32-thread
//!     shared client runtime.
//!
//! Common to both:
//!   - per-packet timestamp in payload; per-thread local Vec of latency
//!     samples merged into shared Vec once on join (matches A/B's
//!     amortized-lock pattern);
//!   - explicit shutdown: close endpoints → grace sleep → drop endpoints
//!     → drop runtimes → join drain threads → compute stats;
//!   - send path uses `try_send` (drop-on-full), matching production.
//!
//! Two unavoidable methodology differences vs A/B:
//!   - Datagrams arrive one-at-a-time; A/B's per-`PacketBatch` recv
//!     timestamp can't apply. C captures recv time per packet — more
//!     accurate per item, slight bias **lower** vs A/B.
//!   - No `SimpleQos` stream-rate throttling (datagrams aren't streams).
//!     Equivalent under the default load (4 packets/slot/peer × 5
//!     slots/sec is well under `max_streams_per_second: 50`).
//!
//! Run:
//!   cargo run --release -p agave-votor --example votor_datagram -- --peers 2000
//!   cargo run --release -p agave-votor --example votor_datagram -- --direction ingress --peers 2000

use {
    bytes::Bytes,
    clap::{Parser, ValueEnum},
    crossbeam_channel::{Receiver, bounded},
    solana_keypair::{Keypair, Signer},
    solana_net_utils::sockets::{bind_to, unique_port_range_for_tests},
    solana_pubkey::Pubkey,
    solana_quic_datagram::{Banlist, admission::AllowAll, endpoint::Datagram},
    std::{
        net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        thread,
        time::{Duration, Instant},
    },
    tokio::runtime::Runtime,
};

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Direction {
    /// 1 client → N servers (egress fan-out).
    Egress,
    /// N clients → 1 server (ingress fan-in).
    Ingress,
}

#[derive(Debug, Parser)]
#[command(about = "quic-datagram fanout benchmark (egress: 1→N, ingress: N→1)")]
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

    /// Accepted for compatibility with the A/B benches' CLI; ignored —
    /// quic-datagram enforces `MAX_DATAGRAMS_PER_SECOND_PER_PEER`.
    #[arg(long, default_value_t = 50)]
    max_streams_per_second: u64,

    /// Datagram payload size in bytes (capped at 1200, the safe max for the default 1280 MTU).
    #[arg(long, default_value_t = 500)]
    packet_size: usize,

    /// Connections to open per warmup batch.
    #[arg(long, default_value_t = 128)]
    handshake_batch: usize,

    /// Pause between warmup batches in milliseconds.
    #[arg(long, default_value_t = 100)]
    handshake_pause_ms: u64,

    /// Test direction (egress = 1→N, ingress = N→1).
    #[arg(long, value_enum, default_value_t = Direction::Egress)]
    direction: Direction,
}

/// Payload layout: `[u32 LE slot][u64 LE ns_since_anchor][padding]` —
/// identical to the A/B benches.
fn encode_packet(slot: u32, anchor: Instant, payload_size: usize) -> Bytes {
    let mut buf = vec![0u8; payload_size];
    buf[0..4].copy_from_slice(&slot.to_le_bytes());
    let ns = anchor.elapsed().as_nanos() as u64;
    buf[4..12].copy_from_slice(&ns.to_le_bytes());
    Bytes::from(buf)
}

fn decode_packet(data: &[u8]) -> Option<(u32, u64)> {
    if data.len() < 12 {
        return None;
    }
    let slot = u32::from_le_bytes(data[0..4].try_into().ok()?);
    let ns = u64::from_le_bytes(data[4..12].try_into().ok()?);
    Some((slot, ns))
}

/// Sentinel slot value used to encode the warmup-batch probe datagrams.
/// Drain threads recognise this and skip the probe entirely (no
/// `received_count` bump, no latency sample) so the post-run delivery
/// rate is computed against real measured traffic only. Picked at the
/// top of the `u32` range so it can't collide with a real slot index.
const PROBE_SLOT: u32 = u32::MAX;

/// Bind a UDP socket on localhost via the test port allocator with a
/// retry budget — the allocator's bookkeeping does not guarantee the
/// underlying port is actually free at the OS level.
fn bind_unique() -> UdpSocket {
    const MAX_ATTEMPTS: usize = 16;
    let mut attempts: Vec<(u16, std::io::Error)> = Vec::with_capacity(MAX_ATTEMPTS);
    for _ in 0..MAX_ATTEMPTS {
        let port = unique_port_range_for_tests(1).start;
        match bind_to(IpAddr::V4(Ipv4Addr::LOCALHOST), port) {
            Ok(s) => return s,
            Err(e) => attempts.push((port, e)),
        }
    }
    let detail = attempts
        .iter()
        .map(|(p, e)| format!("  port {p}: {e}"))
        .collect::<Vec<_>>()
        .join("\n");
    panic!("bind UDP failed after {MAX_ATTEMPTS} attempts:\n{detail}");
}

/// Pick a keypair whose pubkey is strictly greater than every entry in
/// `lower_bounds`. The 1→N client→server fan-out relies on this: the
/// client (lower) dials every server (higher) per the lex rule.
fn keypair_above_all(lower_bounds: &[Pubkey]) -> Keypair {
    let upper = lower_bounds.iter().max().copied().unwrap_or_default();
    loop {
        let k = Keypair::new();
        if k.pubkey() > upper {
            return k;
        }
    }
}

/// Symmetric to [`keypair_above_all`]: pick a keypair whose pubkey is
/// strictly less than every entry in `upper_bounds`. Used by the ingress
/// topology so every client (lower) dials the single server (higher) per
/// the lex rule, and no client ever dials another client.
fn keypair_below_all(upper_bounds: &[Pubkey]) -> Keypair {
    let lower = upper_bounds
        .iter()
        .min()
        .copied()
        .unwrap_or(Pubkey::new_from_array([u8::MAX; 32]));
    loop {
        let k = Keypair::new();
        if k.pubkey() < lower {
            return k;
        }
    }
}

struct ServerHandle {
    addr: SocketAddr,
    rx: Receiver<Datagram>,
    endpoint: solana_quic_datagram::endpoint::QuicDatagramEndpoint,
}

fn spawn_endpoint(
    rt: &Runtime,
    keypair: &Keypair,
) -> (
    solana_quic_datagram::endpoint::QuicDatagramEndpoint,
    Receiver<Datagram>,
    SocketAddr,
) {
    let socket = bind_unique();
    let addr = socket.local_addr().expect("local_addr");
    let (tx, rx) = bounded(1 << 15);
    let banlist = Arc::new(Banlist::<Pubkey>::default());
    let endpoint = agave_votor::datagram_endpoint::spawn(
        rt.handle(),
        keypair,
        socket,
        tx,
        Arc::new(AllowAll),
        banlist,
    )
    .expect("endpoint");
    (endpoint, rx, addr)
}

fn print_stats(slots: usize, num_peers: usize, packets_per_slot: usize, latencies: &[f64]) {
    if latencies.is_empty() {
        println!("\nWARNING: no latency samples collected from measured slots");
        return;
    }
    let mut sorted = latencies.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = sorted.len();
    let sum: f64 = sorted.iter().sum();
    let mean = sum / n as f64;
    let var: f64 = sorted.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / n as f64;
    let stddev = var.sqrt();
    let p50 = sorted[n / 2];
    let p99 = sorted[(n as f64 * 0.99) as usize];
    let min = sorted[0];
    let max = sorted[n - 1];
    let measured_expected = (slots * num_peers * packets_per_slot) as u64;

    println!();
    println!("=== Delivery (measured slots only) ===");
    println!("  Packets expected: {measured_expected}");
    println!("  Packets received: {n}");
    println!(
        "  Delivery rate:    {:.2}%",
        (n as f64 / measured_expected as f64) * 100.0
    );
    println!();
    println!("=== Latency (one-way, microseconds) ===");
    println!("  Mean   : {mean:.1}");
    println!("  Stddev : {stddev:.1}");
    println!("  Min    : {min:.1}");
    println!("  P50    : {p50:.1}");
    println!("  P99    : {p99:.1}");
    println!("  Max    : {max:.1}");
}

fn main() {
    let cli = Cli::parse();
    match cli.direction {
        Direction::Egress => run_egress(cli),
        Direction::Ingress => run_ingress(cli),
    }
}

fn run_egress(cli: Cli) {
    let num_peers = cli.peers;
    let slot_ms = cli.slot_ms;
    let slots = cli.slots;
    let warmup_slots = cli.warmup_slots;
    let packets_per_slot = cli.packets_per_slot;
    let packet_size = cli.packet_size.min(1200).max(12);
    let handshake_batch = cli.handshake_batch.max(1);
    let handshake_pause_ms = cli.handshake_pause_ms;

    assert!(packets_per_slot > 0, "packets_per_slot must be ≥ 1");
    let slot_duration = Duration::from_millis(slot_ms);
    let burst_interval = slot_duration / packets_per_slot as u32;
    let warmup_bursts = warmup_slots * packets_per_slot;
    let total_bursts = (warmup_slots + slots) * packets_per_slot;

    println!(
        "votor_datagram EGRESS: peers={num_peers} slot_ms={slot_ms} slots={slots} \
         warmup={warmup_slots} packets_per_slot={packets_per_slot} packet_size={packet_size} \
         (burst_interval={:.1}ms, total_bursts={total_bursts})",
        burst_interval.as_secs_f64() * 1000.0,
    );

    // Match bench A's runtime split: a small client pool (4 threads, same
    // as `VotorQuicClient::spawn_runtime`) and a larger shared server pool
    // (32 threads). CPU is largely idle during this workload — a single
    // 48-thread pool just adds park/unpark overhead.
    let client_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .thread_name("votor-bench-client")
        .build()
        .expect("client tokio runtime");
    let server_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(32)
        .enable_all()
        .thread_name("votor-bench-server")
        .build()
        .expect("server tokio runtime");

    // Client first, then N servers whose pubkeys are guaranteed to be
    // above the client's so the lex rule routes every dial in one
    // direction.
    let client_keypair = Keypair::new();
    let client_pubkey = client_keypair.pubkey();
    let server_keypairs: Vec<Keypair> = (0..num_peers)
        .map(|_| keypair_above_all(&[client_pubkey]))
        .collect();

    let (client, _client_rx, _client_addr) = spawn_endpoint(&client_rt, &client_keypair);

    println!("spawning {num_peers} servers...");
    let spawn_start = Instant::now();
    let mut servers: Vec<ServerHandle> = Vec::with_capacity(num_peers);
    for (idx, kp) in server_keypairs.iter().enumerate() {
        let (endpoint, rx, addr) = spawn_endpoint(&server_rt, kp);
        servers.push(ServerHandle { addr, rx, endpoint });
        if (idx + 1) % 100 == 0 {
            println!("  spawned {}/{} servers", idx + 1, num_peers);
        }
    }
    println!(
        "1 client + {num_peers} servers spawned in {:.2}s",
        spawn_start.elapsed().as_secs_f64()
    );
    thread::sleep(Duration::from_secs(1));

    let anchor = Instant::now();
    let received_count = Arc::new(AtomicU64::new(0));
    let latencies: Arc<std::sync::Mutex<Vec<f64>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let warmup_u32 = warmup_bursts as u32;

    // Drain threads: shard all N server receivers across `DRAIN_THREADS`
    // OS threads. Each thread owns a contiguous chunk and uses
    // `crossbeam_channel::Select` to block on whichever of its channels
    // becomes ready. Sharding keeps the per-call `Select` scan bounded
    // (~N/K) while avoiding the 2000-threads-fighting-for-CPU pattern of
    // one-thread-per-server. Each thread accumulates samples in a local
    // Vec and merges once on join (matches A/B's amortized-lock pattern).
    //
    // Shutdown: when the bench drops the runtimes, every server's
    // ingress sender is dropped and `recv()` starts returning Err. A
    // drain thread treats the first Err on any of its channels as a
    // shutdown signal, drains remaining packets from its shard via
    // `try_recv`, and exits.
    const DRAIN_THREADS: usize = 4;
    let rxs: Vec<Receiver<Datagram>> = servers.iter().map(|s| s.rx.clone()).collect();
    let shard_size = rxs.len().div_ceil(DRAIN_THREADS).max(1);
    let drain_handles: Vec<thread::JoinHandle<()>> = rxs
        .chunks(shard_size)
        .enumerate()
        .map(|(shard_idx, chunk)| {
            let shard: Vec<Receiver<Datagram>> = chunk.to_vec();
            let received_count = received_count.clone();
            let latencies = latencies.clone();
            thread::Builder::new()
                .name(format!("votor-bench-drain-{shard_idx}"))
                .spawn(move || {
                    let mut sel = crossbeam_channel::Select::new();
                    for rx in &shard {
                        sel.recv(rx);
                    }
                    let mut local_latencies: Vec<f64> = Vec::new();
                    let record = |bytes: &Bytes, local: &mut Vec<f64>| {
                        let recv_ns = anchor.elapsed().as_nanos() as u64;
                        if let Some((slot, send_ns)) = decode_packet(bytes) {
                            if slot == PROBE_SLOT {
                                return;
                            }
                            received_count.fetch_add(1, Ordering::Relaxed);
                            if slot >= warmup_u32 {
                                let us = recv_ns.saturating_sub(send_ns) as f64 / 1000.0;
                                local.push(us);
                            }
                        }
                    };
                    loop {
                        let oper = sel.select();
                        let idx = oper.index();
                        match oper.recv(&shard[idx]) {
                            Ok(dg) => record(&dg.message, &mut local_latencies),
                            Err(_) => {
                                for rx in &shard {
                                    while let Ok(dg) = rx.try_recv() {
                                        record(&dg.message, &mut local_latencies);
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

    let dropped_count = Arc::new(AtomicU64::new(0));

    // Warm up connections in batches — same pattern as A/B. Probes are
    // encoded with `slot = PROBE_SLOT` so the drain threads can filter
    // them out of `received_count` (they're not part of the test traffic
    // and otherwise inflate the delivery rate above 100%).
    println!("establishing connections to {num_peers} servers...");
    {
        let num_batches = num_peers.div_ceil(handshake_batch);
        let probe = encode_packet(PROBE_SLOT, anchor, packet_size);
        for (i, (chunk_kp, chunk_srv)) in server_keypairs
            .chunks(handshake_batch)
            .zip(servers.chunks(handshake_batch))
            .enumerate()
        {
            for (kp, srv) in chunk_kp.iter().zip(chunk_srv.iter()) {
                if client
                    .egress
                    .try_send(Datagram {
                        peer_pubkey: kp.pubkey(),
                        peer_address: srv.addr,
                        message: probe.clone(),
                    })
                    .is_err()
                {
                    dropped_count.fetch_add(1, Ordering::Relaxed);
                }
            }
            println!(
                "  batch {}/{num_batches}: {} connections",
                i + 1,
                chunk_kp.len()
            );
            if i + 1 < num_batches && handshake_pause_ms > 0 {
                thread::sleep(Duration::from_millis(handshake_pause_ms));
            }
        }
        thread::sleep(Duration::from_secs(2));
    }
    println!("connection establishment complete.");

    println!(
        "starting {total_bursts} bursts ({warmup_bursts} warmup + {} measured), {} \
         pkt/peer/burst...",
        slots * packets_per_slot,
        1,
    );
    for burst in 0..total_bursts as u32 {
        let burst_start = Instant::now();
        let is_warmup = (burst as usize) < warmup_bursts;
        let buf = encode_packet(burst, anchor, packet_size);
        for (kp, srv) in server_keypairs.iter().zip(servers.iter()) {
            if client
                .egress
                .try_send(Datagram {
                    peer_pubkey: kp.pubkey(),
                    peer_address: srv.addr,
                    message: buf.clone(),
                })
                .is_err()
            {
                dropped_count.fetch_add(1, Ordering::Relaxed);
            }
        }
        let send_elapsed = burst_start.elapsed();
        if send_elapsed < burst_interval {
            thread::sleep(burst_interval - send_elapsed);
        }
        let expected = num_peers;
        let so_far = received_count.load(Ordering::Relaxed);
        println!(
            "burst {burst}{}: send phase {:.1}ms, expected {expected} packets, received so far: \
             {so_far}",
            if is_warmup { " (warmup)" } else { "" },
            send_elapsed.as_secs_f64() * 1000.0,
        );
    }

    println!("waiting for in-flight packets...");
    thread::sleep(Duration::from_secs(3));

    let total_received = received_count.load(Ordering::Relaxed);
    let total_dropped = dropped_count.load(Ordering::Relaxed);
    let total_expected = (total_bursts * num_peers) as u64;
    println!(
        "total packets: expected={total_expected}, sent={}, dropped_at_egress={total_dropped}, \
         received={total_received}, delivery_rate={:.2}%",
        total_expected.saturating_sub(total_dropped),
        (total_received as f64 / total_expected as f64) * 100.0,
    );

    // Shutdown: close endpoints to break QUIC connections (drops the
    // sender clones held by per-connection read tasks), then drop the
    // runtime to abort any lingering tasks and release whatever senders
    // remain. After that the receiver threads' `rx.recv()` returns Err
    // and they exit, merging their local Vec into `latencies`.
    println!("shutting down client + servers...");
    client.close();
    for srv in &servers {
        srv.endpoint.close();
    }
    drop(servers);
    drop(client);
    drop(client_rt);
    drop(server_rt);

    for h in drain_handles {
        let _ = h.join();
    }

    let latencies = latencies.lock().expect("lock");
    print_stats(slots, num_peers, packets_per_slot, &latencies);
}

fn run_ingress(cli: Cli) {
    let num_peers = cli.peers; // here = number of clients
    let slot_ms = cli.slot_ms;
    let slots = cli.slots;
    let warmup_slots = cli.warmup_slots;
    let packets_per_slot = cli.packets_per_slot;
    let packet_size = cli.packet_size.min(1200).max(12);
    let handshake_batch = cli.handshake_batch.max(1);
    let handshake_pause_ms = cli.handshake_pause_ms;

    assert!(packets_per_slot > 0, "packets_per_slot must be ≥ 1");
    let slot_duration = Duration::from_millis(slot_ms);
    let burst_interval = slot_duration / packets_per_slot as u32;
    let warmup_bursts = warmup_slots * packets_per_slot;
    let total_bursts = (warmup_slots + slots) * packets_per_slot;

    println!(
        "votor_datagram INGRESS: clients={num_peers} slot_ms={slot_ms} slots={slots} \
         warmup={warmup_slots} packets_per_slot={packets_per_slot} packet_size={packet_size} \
         (burst_interval={:.1}ms, total_bursts={total_bursts})",
        burst_interval.as_secs_f64() * 1000.0,
    );

    // Flip of egress: 4-thread server, 32-thread shared client pool.
    let server_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .thread_name("votor-bench-server")
        .build()
        .expect("server tokio runtime");
    let client_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(32)
        .enable_all()
        .thread_name("votor-bench-client")
        .build()
        .expect("client tokio runtime");

    // Server first, then N clients whose pubkeys are strictly below the
    // server's so the lex rule routes every dial in the client→server
    // direction. No client ever dials another client because no code path
    // ever feeds another client's pubkey to a client's `egress.try_send`.
    let server_keypair = Keypair::new();
    let server_pubkey = server_keypair.pubkey();
    let client_keypairs: Vec<Keypair> = (0..num_peers)
        .map(|_| keypair_below_all(&[server_pubkey]))
        .collect();

    println!("spawning 1 server + {num_peers} clients...");
    let spawn_start = Instant::now();
    let (server, server_rx, server_addr) = spawn_endpoint(&server_rt, &server_keypair);

    let mut clients: Vec<solana_quic_datagram::endpoint::QuicDatagramEndpoint> =
        Vec::with_capacity(num_peers);
    for (idx, kp) in client_keypairs.iter().enumerate() {
        let (endpoint, _rx, _addr) = spawn_endpoint(&client_rt, kp);
        clients.push(endpoint);
        if (idx + 1) % 100 == 0 {
            println!("  spawned {}/{} clients", idx + 1, num_peers);
        }
    }
    println!(
        "1 server + {num_peers} clients spawned in {:.2}s",
        spawn_start.elapsed().as_secs_f64()
    );
    thread::sleep(Duration::from_secs(1));

    let anchor = Instant::now();
    let received_count = Arc::new(AtomicU64::new(0));
    let latencies: Arc<std::sync::Mutex<Vec<f64>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let warmup_u32 = warmup_bursts as u32;

    // Single drain thread on the one server rx — no Select needed.
    let drain_rx = server_rx.clone();
    let drain_received = received_count.clone();
    let drain_latencies = latencies.clone();
    let drain_handle = thread::Builder::new()
        .name("votor-bench-drain".to_string())
        .spawn(move || {
            let mut local_latencies: Vec<f64> = Vec::new();
            let mut record = |bytes: &Bytes| {
                let recv_ns = anchor.elapsed().as_nanos() as u64;
                if let Some((slot, send_ns)) = decode_packet(bytes) {
                    if slot == PROBE_SLOT {
                        return;
                    }
                    drain_received.fetch_add(1, Ordering::Relaxed);
                    if slot >= warmup_u32 {
                        let us = recv_ns.saturating_sub(send_ns) as f64 / 1000.0;
                        local_latencies.push(us);
                    }
                }
            };
            while let Ok(dg) = drain_rx.recv() {
                record(&dg.message);
            }
            while let Ok(dg) = drain_rx.try_recv() {
                record(&dg.message);
            }
            drain_latencies
                .lock()
                .expect("lock")
                .extend_from_slice(&local_latencies);
        })
        .expect("spawn drain thread");

    let dropped_count = Arc::new(AtomicU64::new(0));

    // Warmup probes: every client sends one PROBE_SLOT to server. Same
    // batching pattern as egress, iterating clients (not servers).
    println!("establishing connections from {num_peers} clients to server...");
    {
        let num_batches = num_peers.div_ceil(handshake_batch);
        let probe = encode_packet(PROBE_SLOT, anchor, packet_size);
        for (i, chunk) in clients.chunks(handshake_batch).enumerate() {
            for c in chunk.iter() {
                if c.egress
                    .try_send(Datagram {
                        peer_pubkey: server_pubkey,
                        peer_address: server_addr,
                        message: probe.clone(),
                    })
                    .is_err()
                {
                    dropped_count.fetch_add(1, Ordering::Relaxed);
                }
            }
            println!(
                "  batch {}/{num_batches}: {} connections",
                i + 1,
                chunk.len()
            );
            if i + 1 < num_batches && handshake_pause_ms > 0 {
                thread::sleep(Duration::from_millis(handshake_pause_ms));
            }
        }
        thread::sleep(Duration::from_secs(2));
    }
    println!("connection establishment complete.");

    println!(
        "starting {total_bursts} bursts ({warmup_bursts} warmup + {} measured)...",
        slots * packets_per_slot,
    );
    for burst in 0..total_bursts as u32 {
        let burst_start = Instant::now();
        let is_warmup = (burst as usize) < warmup_bursts;
        let buf = encode_packet(burst, anchor, packet_size);
        for c in clients.iter() {
            if c.egress
                .try_send(Datagram {
                    peer_pubkey: server_pubkey,
                    peer_address: server_addr,
                    message: buf.clone(),
                })
                .is_err()
            {
                dropped_count.fetch_add(1, Ordering::Relaxed);
            }
        }
        let send_elapsed = burst_start.elapsed();
        if send_elapsed < burst_interval {
            thread::sleep(burst_interval - send_elapsed);
        }
        let expected = num_peers;
        let so_far = received_count.load(Ordering::Relaxed);
        println!(
            "burst {burst}{}: send phase {:.1}ms, expected {expected} packets, received so far: \
             {so_far}",
            if is_warmup { " (warmup)" } else { "" },
            send_elapsed.as_secs_f64() * 1000.0,
        );
    }

    println!("waiting for in-flight packets...");
    thread::sleep(Duration::from_secs(3));

    let total_received = received_count.load(Ordering::Relaxed);
    let total_dropped = dropped_count.load(Ordering::Relaxed);
    let total_expected = (total_bursts * num_peers) as u64;
    println!(
        "total packets: expected={total_expected}, sent={}, dropped_at_egress={total_dropped}, \
         received={total_received}, delivery_rate={:.2}%",
        total_expected.saturating_sub(total_dropped),
        (total_received as f64 / total_expected as f64) * 100.0,
    );

    println!("shutting down clients + server...");
    for c in &clients {
        c.close();
    }
    server.close();
    drop(clients);
    drop(server);
    drop(server_rx);
    drop(client_rt);
    drop(server_rt);

    let _ = drain_handle.join();

    let latencies = latencies.lock().expect("lock");
    print_stats(slots, num_peers, packets_per_slot, &latencies);
}
