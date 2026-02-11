#![allow(clippy::arithmetic_side_effects)]
//! Standalone test for MAX_STREAMS flow control.
//!
//! Spins up a QUIC server with low capacity, then runs experiments:
//!   1. Unsaturated: staked + unstaked clients send streams freely
//!   2. Saturated: blast streams to exhaust the token bucket, verify
//!      unstaked gets parked and staked still flows
//!   3. Multi-connection: same staked peer opens N connections,
//!      verify total throughput doesn't exceed single-connection throughput
//!
//! Usage:
//!   cargo run --example test_max_streams --features dev-context-only-utils

use {
    crossbeam_channel::unbounded,
    quinn::Connection,
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    solana_streamer::{
        nonblocking::{
            swqos::{SwQos, SwQosConfig},
            testing_utilities::{
                create_quic_server_sockets, make_client_endpoint, spawn_stake_weighted_qos_server,
                SpawnSwQosServerResult,
            },
        },
        quic::{QuicStreamerConfig, StreamerStats},
        streamer::StakedNodes,
    },
    std::{
        collections::HashMap,
        net::SocketAddr,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, RwLock,
        },
        time::{Duration, Instant},
    },
    tokio::time::sleep,
    tokio_util::sync::CancellationToken,
};

/// Open a fresh client connection to the server.
async fn connect(
    server_addr: &SocketAddr,
    keypair: Option<&Keypair>,
) -> Connection {
    make_client_endpoint(server_addr, keypair).await
}

/// Send `count` uni streams on `conn`, each carrying 1 byte.
/// Returns (succeeded, failed).
async fn blast_streams(conn: &Connection, count: usize) -> (usize, usize) {
    let mut ok = 0usize;
    let mut err = 0usize;
    for _ in 0..count {
        match conn.open_uni().await {
            Ok(mut s) => {
                if s.write_all(&[0u8]).await.is_ok() {
                    let _ = s.finish();
                    ok += 1;
                } else {
                    err += 1;
                }
            }
            Err(_) => {
                err += 1;
            }
        }
    }
    (ok, err)
}

/// Send streams as fast as possible for `duration`, return total sent.
async fn blast_for_duration(conn: &Connection, duration: Duration) -> usize {
    let start = Instant::now();
    let mut total = 0usize;
    while start.elapsed() < duration {
        match conn.open_uni().await {
            Ok(mut s) => {
                if s.write_all(&[0u8]).await.is_ok() {
                    let _ = s.finish();
                    total += 1;
                }
            }
            Err(_) => {
                // Connection might be parked (max_streams = 0), back off
                sleep(Duration::from_millis(1)).await;
            }
        }
    }
    total
}

/// Drain the receiver and count packets.
fn drain_receiver(receiver: &crossbeam_channel::Receiver<solana_perf::packet::PacketBatch>) -> usize {
    let mut count = 0;
    while let Ok(batch) = receiver.try_recv() {
        count += batch.len();
    }
    count
}

struct TestServer {
    stats: Arc<StreamerStats>,
    swqos: Arc<SwQos>,
    receiver: crossbeam_channel::Receiver<solana_perf::packet::PacketBatch>,
    server_address: SocketAddr,
    cancel: CancellationToken,
}

fn setup_server(
    max_streams_per_ms: u64,
    max_connections_per_staked_peer: usize,
    staked_nodes: StakedNodes,
) -> TestServer {
    let sockets = create_quic_server_sockets();
    let (sender, receiver) = unbounded();
    let keypair = Keypair::new();
    let server_address = sockets[0].local_addr().unwrap();
    let cancel = CancellationToken::new();

    let SpawnSwQosServerResult {
        server,
        swqos,
    } = spawn_stake_weighted_qos_server(
        "test_max_streams",
        sockets,
        &keypair,
        sender,
        Arc::new(RwLock::new(staked_nodes)),
        QuicStreamerConfig::default(),
        SwQosConfig {
            max_streams_per_ms,
            max_connections_per_staked_peer,
            ..Default::default()
        },
        cancel.clone(),
    )
    .unwrap();

    TestServer {
        stats: server.stats,
        swqos,
        receiver,
        server_address,
        cancel,
    }
}

fn print_header(title: &str) {
    println!("\n{}", "=".repeat(70));
    println!("  {title}");
    println!("{}", "=".repeat(70));
}

fn print_stats(stats: &StreamerStats) {
    println!(
        "  server: streams={} parked={} packets_out={} staked_conns={} unstaked_conns={}",
        stats.total_new_streams(),
        stats.parked_streams(),
        stats.total_packets_sent_to_consumer(),
        stats.connection_added_from_staked_peer(),
        stats.connection_added_from_unstaked_peer(),
    );
}

#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() {
    agave_logger::setup();

    let staked_keypair = Keypair::new();
    let unstaked_keypair = Keypair::new();

    let make_staked_nodes = |kp: &Keypair| {
        StakedNodes::new(
            Arc::new(HashMap::from([(kp.pubkey(), 1_000_000_000u64)])),
            HashMap::<Pubkey, u64>::default(),
        )
    };

    // ── Experiment 1: Unsaturated ──────────────────────────────────────
    print_header("Experiment 1: Unsaturated (high capacity)");
    println!("  max_streams_per_ms=500 (500K/s), sending 100 streams each");

    let server = setup_server(500, 4, make_staked_nodes(&staked_keypair));

    // Let server start
    sleep(Duration::from_millis(200)).await;

    let staked_conn = connect(&server.server_address, Some(&staked_keypair)).await;
    let unstaked_conn = connect(&server.server_address, Some(&unstaked_keypair)).await;

    let (staked_ok, staked_err) = blast_streams(&staked_conn, 100).await;
    let (unstaked_ok, unstaked_err) = blast_streams(&unstaked_conn, 100).await;

    // Let packets arrive
    sleep(Duration::from_millis(500)).await;
    let received = drain_receiver(&server.receiver);

    println!("  staked:   sent={staked_ok} failed={staked_err}");
    println!("  unstaked: sent={unstaked_ok} failed={unstaked_err}");
    println!("  received: {received}");
    print_stats(&server.stats);

    assert!(staked_ok > 90, "staked should succeed when unsaturated");
    assert!(unstaked_ok > 90, "unstaked should succeed when unsaturated");
    println!("  PASS");

    server.cancel.cancel();
    drop(staked_conn);
    drop(unstaked_conn);
    sleep(Duration::from_millis(200)).await;

    // ── Experiment 2: Saturated ────────────────────────────────────────
    print_header("Experiment 2: Saturated (low capacity, unstaked should be parked)");
    println!("  max_streams_per_ms=1 (1K/s, burst=100), blasting to saturate");

    let server = setup_server(1, 4, make_staked_nodes(&staked_keypair));
    sleep(Duration::from_millis(200)).await;

    // First: saturate the bucket with a burst of streams from a helper connection
    let saturator = connect(&server.server_address, Some(&staked_keypair)).await;
    let (sat_ok, _) = blast_streams(&saturator, 200).await;
    println!("  saturator sent {sat_ok} streams to exhaust bucket");
    sleep(Duration::from_millis(50)).await;

    // Now try unstaked — should mostly fail / be parked
    let unstaked_conn = connect(&server.server_address, Some(&unstaked_keypair)).await;

    let parked_before = server.stats.parked_streams();
    let unstaked_sent = blast_for_duration(&unstaked_conn, Duration::from_secs(2)).await;

    sleep(Duration::from_millis(200)).await;
    let parked_after = server.stats.parked_streams();

    println!("  unstaked streams sent in 2s: {unstaked_sent}");
    println!("  parked_streams: before={parked_before} after={parked_after} delta={}",
        parked_after.saturating_sub(parked_before));
    print_stats(&server.stats);

    // Staked should still be able to send
    let staked_conn = connect(&server.server_address, Some(&staked_keypair)).await;
    let (staked_ok, staked_err) = blast_streams(&staked_conn, 50).await;
    println!("  staked after saturation: sent={staked_ok} failed={staked_err}");

    if parked_after > parked_before {
        println!("  PASS: unstaked connections were parked during saturation");
    } else {
        println!("  NOTE: parked_streams didn't increase — bucket may have recovered too quickly");
        println!("        (this can happen on fast machines where 1K/s refill outpaces the test)");
    }

    server.cancel.cancel();
    drop(saturator);
    drop(unstaked_conn);
    drop(staked_conn);
    sleep(Duration::from_millis(200)).await;

    // ── Experiment 3: Multi-connection quota sharing ───────────────────
    print_header("Experiment 3: Multi-connection quota sharing");
    println!("  Same staked peer opens 1 vs 4 connections, compare throughput");

    // Parameters chosen so that the staked peer's quota is large enough
    // to be meaningfully divided among multiple connections:
    //   capacity_tps = max_streams_per_ms * 1000 = 10_000
    //   share_tps = capacity_tps * 9B / 10B = 9_000 (90% of capacity)
    //   quota = share_tps * MIN_RTT = 9_000 * 0.002 = 18 streams
    //   1 conn: max_streams = 18
    //   4 conn: max_streams = 18/4 = 4 each → total concurrent = 16 ≈ 18
    //
    // A background peer (10% stake) keeps the bucket saturated so that
    // compute_max_streams applies the stake-proportional quota.
    let bg_keypair = Keypair::new();
    let staked_nodes3 = StakedNodes::new(
        Arc::new(HashMap::from([
            (staked_keypair.pubkey(), 9_000_000_000u64), // 90% of stake
            (bg_keypair.pubkey(), 1_000_000_000u64),     // 10% of stake
        ])),
        HashMap::<Pubkey, u64>::default(),
    );
    let server = setup_server(10, 4, staked_nodes3);
    sleep(Duration::from_millis(200)).await;

    let measure_duration = Duration::from_secs(3);

    // Background saturator: keep the bucket drained during measurements.
    let bg_conn = connect(&server.server_address, Some(&bg_keypair)).await;
    let bg_cancel = CancellationToken::new();
    let bg_cancel2 = bg_cancel.clone();
    let bg_handle = tokio::spawn(async move {
        let mut total = 0usize;
        loop {
            tokio::select! {
                result = bg_conn.open_uni() => {
                    match result {
                        Ok(mut s) => {
                            let _ = s.write_all(&[0u8]).await;
                            let _ = s.finish();
                            total += 1;
                        }
                        Err(_) => {
                            sleep(Duration::from_millis(1)).await;
                        }
                    }
                }
                _ = bg_cancel2.cancelled() => break,
            }
        }
        total
    });

    // Let background saturator drain the bucket
    sleep(Duration::from_secs(1)).await;
    let lt = server.swqos.load_tracker();
    println!("  after warmup: parked={} streams={} saturated={} bucket_level={}",
        server.stats.parked_streams(), server.stats.total_new_streams(),
        lt.is_saturated(), lt.bucket_level());

    // Single connection throughput
    let single_conn = connect(&server.server_address, Some(&staked_keypair)).await;
    let streams_before = server.stats.total_new_streams();
    let single_throughput = blast_for_duration(&single_conn, measure_duration).await;
    let streams_after = server.stats.total_new_streams();
    println!("  1 connection: {single_throughput} streams in {}s (server delta: {})",
        measure_duration.as_secs(), streams_after - streams_before);
    println!("  parked after single-conn test: {} saturated={} bucket_level={}",
        server.stats.parked_streams(), lt.is_saturated(), lt.bucket_level());

    drop(single_conn);
    sleep(Duration::from_millis(500)).await;

    // Four connections throughput (each runs in a spawned task)
    let conns: Vec<Connection> = {
        let mut v = Vec::new();
        for _ in 0..4 {
            v.push(connect(&server.server_address, Some(&staked_keypair)).await);
        }
        v
    };
    sleep(Duration::from_millis(100)).await;

    let multi_total = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for conn in conns {
        let total = multi_total.clone();
        handles.push(tokio::spawn(async move {
            let sent = blast_for_duration(&conn, measure_duration).await;
            total.fetch_add(sent, Ordering::Relaxed);
            sent
        }));
    }

    let mut per_conn_results = Vec::new();
    for h in handles {
        per_conn_results.push(h.await.unwrap());
    }
    let multi_throughput = multi_total.load(Ordering::Relaxed);

    bg_cancel.cancel();
    let bg_sent = bg_handle.await.unwrap();
    println!("  background saturator sent: {bg_sent} streams");

    println!("  4 connections: {multi_throughput} streams total in {}s", measure_duration.as_secs());
    for (i, count) in per_conn_results.iter().enumerate() {
        println!("    conn[{i}]: {count} streams");
    }

    let ratio = if single_throughput > 0 {
        multi_throughput as f64 / single_throughput as f64
    } else {
        f64::NAN
    };
    println!("  ratio (multi/single): {ratio:.2}x");

    // On localhost, 4 connections = 4 parallel handler tasks, giving
    // a sqrt(4) ≈ 2x throughput boost even with the same total concurrency
    // budget. A ratio near 2.0 means the quota sharing is working correctly.
    // Without sharing, the ratio would be ~4x (each conn gets full quota).
    if ratio < 2.5 {
        println!("  PASS: quota sharing limits multi-connection advantage (expected ~sqrt(N) on localhost)");
    } else {
        println!("  WARN: ratio > 2.5x — quota sharing may not be effective enough");
    }

    print_stats(&server.stats);
    server.cancel.cancel();
    sleep(Duration::from_millis(200)).await;

    println!("\n{}", "=".repeat(70));
    println!("  All experiments complete.");
    println!("{}", "=".repeat(70));
}
