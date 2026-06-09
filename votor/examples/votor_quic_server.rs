#![allow(clippy::arithmetic_side_effects)]
//! Standalone QUIC server for votor / tpu-client-next benchmarks.
//!
//! Loads N peer definitions from a JSON file, spawns one SimpleQos QUIC
//! server per entry, and waits for packets until Ctrl+C. On exit it
//! prints one-way latency stats (requires CLOCK_MONOTONIC send timestamps
//! encoded at byte offset 4 of each packet — the same wire format used by
//! `vote_quic_client.rs` egress mode and `vote_connection_cache_netns.rs`).
//!
//! Config file format (`--config`):
//! ```json
//! {
//!   "peers": [
//!     { "keypair": [1,2,...,64], "stake_lamports": 1000000,
//!       "ip": "10.0.1.1", "base_port": 8000 }
//!   ]
//! }
//! ```
//!
//! `keypair` is the standard 64-byte Solana ed25519 keypair encoding.
//! Each peer binds exactly one QUIC server on `ip:base_port`.
//!
//! Usage:
//!   cargo run --example votor_quic_server -p agave-votor -- \
//!       --config /home/sol/fake_peers.json

use {
    clap::Parser,
    crossbeam_channel::{Receiver, bounded},
    log::info,
    serde::Deserialize,
    solana_keypair::Keypair,
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
        fs,
        net::{IpAddr, SocketAddr, UdpSocket},
        path::PathBuf,
        sync::{
            Arc, RwLock,
            atomic::{AtomicU64, Ordering},
        },
        thread,
    },
    tokio::task::JoinHandle,
    tokio_util::sync::CancellationToken,
};

/// Warmup sentinel — drain threads skip latency recording for this slot.
const PROBE_SLOT: u32 = u32::MAX;

#[derive(Debug, Deserialize)]
struct PeerConfig {
    /// 64-byte Solana ed25519 keypair (secret || public).
    keypair: Vec<u8>,
    stake_lamports: u64,
    ip: IpAddr,
    base_port: u16,
}

#[derive(Debug, Deserialize)]
struct Config {
    peers: Vec<PeerConfig>,
}

#[derive(Debug, Parser)]
#[command(about = "Standalone QUIC server for votor/tpu-client-next benchmarks")]
struct Cli {
    /// Path to the peer config JSON file.
    #[arg(long)]
    config: PathBuf,

    /// Max streams per second per connection (SimpleQos rate limiter).
    #[arg(long, default_value_t = 50)]
    max_streams_per_second: u64,

    /// Slot index below which packets are excluded from latency stats.
    #[arg(long, default_value_t = 5)]
    warmup_slots: u32,
}

/// Absolute CLOCK_MONOTONIC nanoseconds — shared across network namespaces
/// on the same host kernel, so client send-timestamps are directly
/// comparable to server receive-timestamps.
fn monotonic_nanos() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts is a valid &mut libc::timespec; CLOCK_MONOTONIC is always
    // available on Linux.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    assert_eq!(rc, 0, "clock_gettime(CLOCK_MONOTONIC) failed");
    (ts.tv_sec as u64) * 1_000_000_000 + ts.tv_nsec as u64
}

fn decode_packet(data: &[u8]) -> Option<(u32, u64)> {
    if data.len() < 12 {
        return None;
    }
    let slot = u32::from_le_bytes(data[0..4].try_into().ok()?);
    let send_nanos = u64::from_le_bytes(data[4..12].try_into().ok()?);
    Some((slot, send_nanos))
}

struct ServerSlot {
    handle: JoinHandle<()>,
    receiver: Receiver<PacketBatch>,
}

fn main() -> anyhow::Result<()> {
    agave_logger::setup();
    let cli = Cli::parse();

    let raw = fs::read_to_string(&cli.config)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", cli.config.display()))?;
    let config: Config = serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("parsing {}: {e}", cli.config.display()))?;

    let num_peers = config.peers.len();
    anyhow::ensure!(num_peers > 0, "config.peers is empty");

    // Parse keypairs and build the staked-nodes map.
    let mut keypairs: Vec<Keypair> = Vec::with_capacity(num_peers);
    let mut stakes: HashMap<_, u64> = HashMap::with_capacity(num_peers);
    for (idx, peer) in config.peers.iter().enumerate() {
        let kp = Keypair::try_from(peer.keypair.as_slice()).map_err(|e| {
            anyhow::anyhow!("peers[{idx}].keypair is not a valid 64-byte ed25519 keypair: {e}")
        })?;
        stakes.insert(kp.pubkey(), peer.stake_lamports);
        keypairs.push(kp);
    }
    let staked_nodes = Arc::new(RwLock::new(StakedNodes::new(
        Arc::new(stakes),
        HashMap::default(),
    )));

    let cancel = CancellationToken::new();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(32)
        .enable_all()
        .build()?;
    let _guard = runtime.enter();

    info!(
        "Spawning {num_peers} QUIC servers from {}...",
        cli.config.display()
    );

    let mut servers: Vec<ServerSlot> = Vec::with_capacity(num_peers);
    for (idx, (keypair, peer)) in keypairs.iter().zip(config.peers.iter()).enumerate() {
        let addr = SocketAddr::new(peer.ip, peer.base_port);
        let sock =
            UdpSocket::bind(addr).map_err(|e| anyhow::anyhow!("peers[{idx}]: bind {addr}: {e}"))?;
        let (sender, receiver) = bounded(4096);

        let (SpawnNonBlockingServerResult { thread: handle, .. }, _banlist) =
            spawn_simple_qos_server(
                "votor_quic_server",
                [QuicSocket::Kernel(sock)],
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
                cancel.clone(),
            )?;

        servers.push(ServerSlot { handle, receiver });

        if (idx + 1) % 100 == 0 {
            info!("  spawned {}/{}", idx + 1, num_peers);
        }
    }

    info!("All {num_peers} servers ready — waiting for Ctrl+C");

    let received_count = Arc::new(AtomicU64::new(0));
    let latencies: Arc<std::sync::Mutex<Vec<f64>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

    // Shard N receivers across K=4 OS threads with crossbeam Select to
    // avoid one-thread-per-server wakeup churn.
    const DRAIN_THREADS: usize = 4;
    let rxs: Vec<Receiver<PacketBatch>> = servers.iter().map(|s| s.receiver.clone()).collect();
    let shard_size = rxs.len().div_ceil(DRAIN_THREADS).max(1);
    let warmup_threshold = cli.warmup_slots;
    let receiver_handles: Vec<thread::JoinHandle<()>> = rxs
        .chunks(shard_size)
        .enumerate()
        .map(|(shard_idx, chunk)| {
            let shard: Vec<Receiver<PacketBatch>> = chunk.to_vec();
            let latencies = latencies.clone();
            let received_count = received_count.clone();
            thread::Builder::new()
                .name(format!("drain-{shard_idx}"))
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
                                    let us = recv_nanos.saturating_sub(send_nanos) as f64 / 1000.0;
                                    local.push(us);
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
                        .expect("lock poisoned")
                        .extend_from_slice(&local_latencies);
                })
                .expect("spawn drain thread")
        })
        .collect();

    // Block until Ctrl+C.
    runtime.block_on(tokio::signal::ctrl_c()).ok();
    info!("Ctrl+C — shutting down...");

    cancel.cancel();
    for server in servers.drain(..) {
        drop(server.receiver);
        runtime.block_on(async {
            let _ = server.handle.await;
        });
    }
    for h in receiver_handles {
        let _ = h.join();
    }

    let total_received = received_count.load(Ordering::Relaxed);
    info!("Total packets received: {total_received}");

    let latencies = latencies.lock().expect("lock poisoned");
    if latencies.is_empty() {
        println!("No measured latency samples (all traffic was warmup/probe).");
        return Ok(());
    }
    let n = latencies.len();
    let mean = latencies.iter().sum::<f64>() / n as f64;
    let variance = latencies
        .iter()
        .map(|v| (v - mean) * (v - mean))
        .sum::<f64>()
        / n as f64;
    let stddev = variance.sqrt();
    let mut sorted = latencies.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());

    println!("\n=== Latency Statistics (microseconds, one-way CLOCK_MONOTONIC) ===");
    println!("  Samples: {n}");
    println!("  Mean:    {mean:.1} us");
    println!("  Stddev:  {stddev:.1} us");
    println!("  Min:     {:.1} us", sorted[0]);
    println!("  P50:     {:.1} us", sorted[n / 2]);
    println!("  P99:     {:.1} us", sorted[(n as f64 * 0.99) as usize]);
    println!("  Max:     {:.1} us", sorted[n - 1]);

    Ok(())
}
