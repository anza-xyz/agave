#![allow(clippy::arithmetic_side_effects)]
//! QUIC fanout benchmark using the old ConnectionCache approach.
//!
//! This is the "before" counterpart to vote_quic_client (the "after").
//! It spawns one ConnectionCache client and N SimpleQos QUIC servers.
//! On each "slot tick", the client sends `packets_per_slot` packets to every
//! server. The benchmark validates delivery and reports latency statistics.
//!
//! Usage:
//!   cargo run --example vote_connection_cache -p agave-votor -- --peers 100 --slot-ms 200 --slots 5

use {
    clap::Parser,
    crossbeam_channel::{Receiver, bounded},
    log::info,
    solana_client::connection_cache::ConnectionCache,
    solana_connection_cache::client_connection::ClientConnection,
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

#[derive(Debug, Parser)]
#[command(about = "ConnectionCache fanout benchmark (1 client → N servers) [OLD approach]")]
struct Cli {
    /// Number of server peers
    #[arg(long, default_value_t = 20)]
    peers: usize,

    /// Slot duration in milliseconds
    #[arg(long, default_value_t = 200)]
    slot_ms: u64,

    /// Number of slots to run
    #[arg(long, default_value_t = 50)]
    slots: usize,

    /// Packets the client sends to each server per slot
    #[arg(long, default_value_t = 4)]
    packets_per_slot: usize,

    /// Warmup slots (not counted in stats)
    #[arg(long, default_value_t = 5)]
    warmup_slots: usize,

    /// Max streams per second per connection for SimpleQos
    #[arg(long, default_value_t = 50)]
    max_streams_per_second: u64,
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

fn main() -> anyhow::Result<()> {
    agave_logger::setup();
    let cli = Cli::parse();

    let num_peers = cli.peers;
    assert!(cli.packets_per_slot > 0, "packets_per_slot must be ≥ 1");
    let slot_duration = Duration::from_millis(cli.slot_ms);
    let burst_interval = slot_duration / cli.packets_per_slot as u32;
    let warmup_bursts = cli.warmup_slots * cli.packets_per_slot;
    let total_bursts = (cli.warmup_slots + cli.slots) * cli.packets_per_slot;

    info!(
        "Starting ConnectionCache fanout benchmark: 1 client → {} servers, {}ms slots, {} \
         measurement slots, {} packets/slot/server (burst_interval={:.1}ms, total_bursts={})",
        num_peers,
        cli.slot_ms,
        cli.slots,
        cli.packets_per_slot,
        burst_interval.as_secs_f64() * 1000.0,
        total_bursts,
    );

    // Generate keypairs: one per server (ConnectionCache doesn't need a client keypair for basic sends)
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

    // Server runtime: 32 threads for all QUIC servers.
    let server_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(32)
        .enable_all()
        .build()?;
    let _server_guard = server_runtime.enter();

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
            "conn_cache_bench",
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

    // Create the ConnectionCache client (old approach).
    // Uses a QUIC ConnectionCache with a pool size of 10, matching old votor usage.
    let client_socket = bind_to_localhost_unique()?;
    let connection_cache = Arc::new(ConnectionCache::new_with_client_options(
        "BenchAlpenglowConnectionCache",
        1,
        Some(client_socket),
        Some((&client_keypair, std::net::Ipv4Addr::LOCALHOST.into())),
        Some((&staked_nodes, &client_keypair.pubkey())),
    ));

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
            for addr in chunk {
                let conn = connection_cache.get_connection(addr);
                let _ = conn.send_data_async(Arc::new(buf.clone()));
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
        // Wait for handshakes to complete.
        thread::sleep(Duration::from_secs(2));
        info!("Connection establishment complete.");
    }

    info!(
        "Starting {} bursts ({} warmup + {} measured)...",
        total_bursts,
        warmup_bursts,
        cli.slots * cli.packets_per_slot,
    );

    // Burst loop — mirrors VotingService::broadcast_consensus_message but
    // emits one packet per peer per burst, with bursts spaced
    // `burst_interval` apart (=`slot_ms / packets_per_slot`).
    for burst in 0..total_bursts as u32 {
        let burst_start = Instant::now();
        let is_warmup = (burst as usize) < warmup_bursts;

        let buf = encode_packet(burst, epoch);
        for addr in &all_addrs {
            let conn = connection_cache.get_connection(addr);
            let _ = conn.send_data_async(Arc::new(buf.clone()));
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

    // Shutdown
    info!("Shutting down client...");
    drop(connection_cache);

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

    // Compute latency statistics (only from measured slots)
    let latencies = latencies.lock().expect("lock");
    if !latencies.is_empty() {
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

        let measured_expected = (cli.slots * num_peers * cli.packets_per_slot) as u64;

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
    } else {
        println!("\nWARNING: No latency samples collected from measured slots!");
    }

    info!("Done.");
    Ok(())
}
