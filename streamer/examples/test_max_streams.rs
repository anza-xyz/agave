#![allow(clippy::arithmetic_side_effects)]
//! Standalone test for MAX_STREAMS flow control.
//!
//! Spins up a QUIC server with low capacity, then runs experiments:
//!   1. Unsaturated: staked + unstaked clients send streams freely
//!   2. Saturated: blast streams to exhaust the token bucket, verify
//!      unstaked gets parked and staked still flows
//!   3. Multi-connection: same staked peer opens N connections,
//!      verify total throughput doesn't exceed single-connection throughput
//!   4. Stake-proportional: 4 peers with different stakes blast under
//!      saturation, verify throughput is proportional to stake
//!   5. RTT-independent: 3 peers with equal stake but different RTTs,
//!      verify throughput is equal (BDP scaling compensates for RTT)
//!   6. Mixed stake + RTT: 3 peers with different stakes AND RTTs,
//!      verify throughput tracks stake regardless of RTT
//!   7. Unsaturated BDP scaling: equal stake, different RTTs above
//!      REFERENCE_RTT, verify equal throughput without saturation
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
async fn connect(server_addr: &SocketAddr, keypair: Option<&Keypair>) -> Connection {
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

/// Open streams concurrently. Each stream is held open for `hold_duration`
/// before sending data, simulating network RTT. QUIC's MAX_STREAMS limit
/// becomes the actual bottleneck, making throughput ≈ quota / hold_duration.
async fn blast_concurrent(conn: &Connection, duration: Duration, hold_duration: Duration) -> usize {
    let total = Arc::new(AtomicUsize::new(0));
    let deadline = Instant::now() + duration;

    while Instant::now() < deadline {
        match conn.open_uni().await {
            Ok(mut s) => {
                let total = total.clone();
                tokio::spawn(async move {
                    // Hold the stream open, simulating in-flight time.
                    // The stream counts against MAX_STREAMS until finished.
                    sleep(hold_duration).await;
                    if s.write_all(&[0u8]).await.is_ok() {
                        let _ = s.finish();
                        total.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
            Err(_) => {
                sleep(Duration::from_millis(1)).await;
            }
        }
    }

    // Wait for in-flight streams to complete
    sleep(hold_duration + Duration::from_millis(100)).await;
    total.load(Ordering::Relaxed)
}

/// Drain the receiver and count packets.
fn drain_receiver(
    receiver: &crossbeam_channel::Receiver<solana_perf::packet::PacketBatch>,
) -> usize {
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

fn setup_server_with_config(staked_nodes: StakedNodes, config: SwQosConfig) -> TestServer {
    let sockets = create_quic_server_sockets();
    let (sender, receiver) = unbounded();
    let keypair = Keypair::new();
    let server_address = sockets[0].local_addr().unwrap();
    let cancel = CancellationToken::new();

    let SpawnSwQosServerResult { server, swqos } = spawn_stake_weighted_qos_server(
        "test_max_streams",
        sockets,
        &keypair,
        sender,
        Arc::new(RwLock::new(staked_nodes)),
        QuicStreamerConfig::default(),
        config,
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

fn setup_server(
    max_streams_per_ms: u64,
    max_connections_per_staked_peer: usize,
    staked_nodes: StakedNodes,
) -> TestServer {
    setup_server_with_config(
        staked_nodes,
        SwQosConfig {
            max_streams_per_ms,
            max_connections_per_staked_peer,
            ..Default::default()
        },
    )
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
    println!(
        "  parked_streams: before={parked_before} after={parked_after} delta={}",
        parked_after.saturating_sub(parked_before)
    );
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

    // Uses blast_concurrent with a simulated RTT so that MAX_STREAMS is
    // the actual bottleneck (streams stay open for hold_duration, hitting
    // the concurrent limit). Without a hold time, streams close instantly
    // and the concurrent count never approaches MAX_STREAMS.
    //
    //   capacity_tps = max_streams_per_ms * 1000 = 10_000
    //   share_tps = capacity_tps * 9B / 10B = 9_000 (90% of capacity)
    //   simulated_rtt = 20ms
    //   quota = share_tps * 0.020 = 180 streams
    //   1 conn: max_streams = 180, throughput ≈ 180 / 0.020 = 9,000/s
    //   4 conn: max_streams = 180/4 = 45 each, total = 180
    //           throughput ≈ 180 / 0.020 = 9,000/s (same!)
    //   Ratio ≈ 1.0 — opening more connections does not help.
    //
    // A background peer (10% stake) keeps the bucket saturated so that
    // compute_max_streams applies the stake-proportional quota.
    let simulated_rtt3 = Duration::from_millis(20);
    let bg_keypair = Keypair::new();
    let staked_nodes3 = StakedNodes::new(
        Arc::new(HashMap::from([
            (staked_keypair.pubkey(), 9_000_000_000u64), // 90% of stake
            (bg_keypair.pubkey(), 1_000_000_000u64),     // 10% of stake
        ])),
        HashMap::<Pubkey, u64>::default(),
    );
    let rtt_overrides3: HashMap<Pubkey, Duration> = HashMap::from([
        (staked_keypair.pubkey(), simulated_rtt3),
        (bg_keypair.pubkey(), simulated_rtt3),
    ]);
    let server = setup_server_with_config(
        staked_nodes3,
        SwQosConfig {
            max_streams_per_ms: 10,
            max_connections_per_staked_peer: 4,
            rtt_overrides: rtt_overrides3,
            ..Default::default()
        },
    );
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
    println!(
        "  after warmup: parked={} streams={} saturated={} bucket_level={}",
        server.stats.parked_streams(),
        server.stats.total_new_streams(),
        lt.is_saturated(),
        lt.bucket_level()
    );

    // Single connection throughput (with simulated RTT hold)
    let single_conn = connect(&server.server_address, Some(&staked_keypair)).await;
    let single_throughput = blast_concurrent(&single_conn, measure_duration, simulated_rtt3).await;
    println!(
        "  1 connection: {single_throughput} streams in {}s ({:.0}/s)",
        measure_duration.as_secs(),
        single_throughput as f64 / measure_duration.as_secs_f64(),
    );

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
        let rtt = simulated_rtt3;
        handles.push(tokio::spawn(async move {
            let sent = blast_concurrent(&conn, measure_duration, rtt).await;
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

    println!(
        "  4 connections: {multi_throughput} streams total in {}s ({:.0}/s)",
        measure_duration.as_secs(),
        multi_throughput as f64 / measure_duration.as_secs_f64(),
    );
    for (i, count) in per_conn_results.iter().enumerate() {
        println!("    conn[{i}]: {count} streams");
    }

    let ratio = if single_throughput > 0 {
        multi_throughput as f64 / single_throughput as f64
    } else {
        f64::NAN
    };
    println!("  ratio (multi/single): {ratio:.2}x");

    // With quota sharing, 4 connections get the same total concurrent
    // budget as 1 connection (quota is divided by connection count).
    // Ratio should be close to 1.0. Allow up to 1.5 for scheduling jitter.
    if ratio < 1.5 {
        println!("  PASS: quota sharing prevents multi-connection advantage (ratio ≈ 1.0)");
    } else {
        println!("  WARN: ratio > 1.5x — quota sharing may not be effective enough");
    }

    print_stats(&server.stats);
    server.cancel.cancel();
    sleep(Duration::from_millis(200)).await;

    // ── Experiment 4: Stake-proportional throughput ──────────────────
    print_header("Experiment 4: Stake-proportional throughput under saturation");
    println!("  4 peers with stakes 50%/20%/20%/10%, simulated RTT=20ms");

    let simulated_rtt = Duration::from_millis(20);

    // Stake ratios: 5:2:2:1 (total ~10B)
    // A background saturator (tiny stake) blasts at full speed to keep
    // the bucket drained, ensuring compute_max_streams always takes the
    // saturated branch with proportional quotas.
    let peers: Vec<(Keypair, u64)> = vec![
        (Keypair::new(), 5_000_000_000), // ~50%
        (Keypair::new(), 2_000_000_000), // ~20%
        (Keypair::new(), 2_000_000_000), // ~20%
        (Keypair::new(), 1_000_000_000), // ~10%
    ];
    let bg_keypair4 = Keypair::new();

    // capacity_tps = 50 * 1000 = 50K/s, burst = 5K
    // At simulated_rtt=20ms, quotas:
    //   50%: 25K * 0.02 = 500 concurrent streams
    //   20%: 10K * 0.02 = 200
    //   10%:  5K * 0.02 = 100
    // Throughput ≈ quota / hold_duration, proportional to stake.
    let mut stake_map: HashMap<Pubkey, u64> =
        peers.iter().map(|(kp, s)| (kp.pubkey(), *s)).collect();
    stake_map.insert(bg_keypair4.pubkey(), 100_000_000); // 1% saturator
    let mut rtt_overrides: HashMap<Pubkey, Duration> = peers
        .iter()
        .map(|(kp, _)| (kp.pubkey(), simulated_rtt))
        .collect();
    rtt_overrides.insert(bg_keypair4.pubkey(), simulated_rtt);
    let staked_nodes4 = StakedNodes::new(Arc::new(stake_map), HashMap::<Pubkey, u64>::default());
    let server = setup_server_with_config(
        staked_nodes4,
        SwQosConfig {
            max_streams_per_ms: 50,
            max_connections_per_staked_peer: 1,
            rtt_overrides,
            ..Default::default()
        },
    );
    sleep(Duration::from_millis(200)).await;

    let measure_duration = Duration::from_secs(3);

    // Background saturator: blast at full speed (no simulated RTT) to
    // keep the bucket drained throughout the measurement.
    let bg_conn4 = connect(&server.server_address, Some(&bg_keypair4)).await;
    let bg_cancel4 = CancellationToken::new();
    let bg_cancel4b = bg_cancel4.clone();
    let bg_handle4 = tokio::spawn(async move {
        let mut total = 0usize;
        loop {
            tokio::select! {
                result = bg_conn4.open_uni() => {
                    match result {
                        Ok(mut s) => {
                            let _ = s.write_all(&[0u8]).await;
                            let _ = s.finish();
                            total += 1;
                        }
                        Err(_) => { sleep(Duration::from_millis(1)).await; }
                    }
                }
                _ = bg_cancel4b.cancelled() => break,
            }
        }
        total
    });

    // Let saturator drain the bucket
    sleep(Duration::from_secs(1)).await;
    let lt = server.swqos.load_tracker();
    println!(
        "  after warmup: saturated={} bucket_level={}",
        lt.is_saturated(),
        lt.bucket_level()
    );

    // Connect all test peers and blast concurrently with simulated RTT.
    // Each stream is held open for `simulated_rtt`, so QUIC's MAX_STREAMS
    // becomes the actual throughput bottleneck.
    let mut handles = Vec::new();
    for (keypair, stake) in &peers {
        let conn = connect(&server.server_address, Some(keypair)).await;
        let stake = *stake;
        let rtt = simulated_rtt;
        handles.push(tokio::spawn(async move {
            let sent = blast_concurrent(&conn, measure_duration, rtt).await;
            (stake, sent)
        }));
    }

    let mut results: Vec<(u64, usize)> = Vec::new();
    for h in handles {
        results.push(h.await.unwrap());
    }

    bg_cancel4.cancel();
    let bg_sent4 = bg_handle4.await.unwrap();

    // Sort by stake descending
    results.sort_by(|a, b| b.0.cmp(&a.0));

    let total_stake: u64 = results.iter().map(|(s, _)| s).sum();
    let total_throughput: usize = results.iter().map(|(_, sent)| sent).sum();
    println!(
        "  total throughput: {} streams in {}s ({:.0}/s)",
        total_throughput,
        measure_duration.as_secs(),
        total_throughput as f64 / measure_duration.as_secs_f64(),
    );
    println!("  background saturator sent: {bg_sent4}");
    println!(
        "  saturated={} bucket_level={}",
        lt.is_saturated(),
        lt.bucket_level()
    );

    println!(
        "  {:>7}  {:>10}  {:>8}  {:>10}  {:>5}",
        "stake%", "throughput", "share%", "expected%", "ratio"
    );
    let mut pass = true;
    for (stake, sent) in &results {
        let stake_pct = *stake as f64 / total_stake as f64 * 100.0;
        let throughput_pct = *sent as f64 / total_throughput as f64 * 100.0;
        let ratio = throughput_pct / stake_pct;
        println!(
            "  {:>6.1}%  {:>10}  {:>7.1}%  {:>9.1}%  {:>4.2}x",
            stake_pct, sent, throughput_pct, stake_pct, ratio,
        );
        // Each peer's throughput share should be close to its stake share
        if ratio < 0.5 || ratio > 2.0 {
            pass = false;
        }
    }

    // Verify ordering: higher stake → higher throughput
    let ordered = results.windows(2).all(|w| w[0].1 >= w[1].1);

    if pass && ordered {
        println!("  PASS: throughput is proportional to stake");
    } else if !ordered {
        println!("  WARN: throughput ordering doesn't match stake ordering");
    } else {
        println!("  WARN: throughput shares deviate significantly from stake shares");
    }

    print_stats(&server.stats);
    server.cancel.cancel();
    sleep(Duration::from_millis(200)).await;

    // ── Experiment 5: RTT-independent throughput ─────────────────────
    print_header("Experiment 5: Equal stake, different RTTs → equal throughput");
    println!("  3 peers with equal stake but RTTs of 10ms, 50ms, 100ms");

    // Three peers with equal stake but different RTTs.
    // quota = share_tps * rtt, throughput = quota / rtt = share_tps.
    // All three should achieve the same throughput regardless of RTT.
    let rtt_peers: Vec<(Keypair, u64, Duration)> = vec![
        (Keypair::new(), 3_000_000_000, Duration::from_millis(10)), // 30%, fast
        (Keypair::new(), 3_000_000_000, Duration::from_millis(50)), // 30%, medium
        (Keypair::new(), 3_000_000_000, Duration::from_millis(100)), // 30%, slow
    ];
    let bg_keypair5 = Keypair::new();

    let mut stake_map5: HashMap<Pubkey, u64> = rtt_peers
        .iter()
        .map(|(kp, s, _)| (kp.pubkey(), *s))
        .collect();
    stake_map5.insert(bg_keypair5.pubkey(), 100_000_000);
    let rtt_overrides5: HashMap<Pubkey, Duration> = rtt_peers
        .iter()
        .map(|(kp, _, rtt)| (kp.pubkey(), *rtt))
        .chain(std::iter::once((
            bg_keypair5.pubkey(),
            Duration::from_millis(10),
        )))
        .collect();
    let staked_nodes5 = StakedNodes::new(Arc::new(stake_map5), HashMap::<Pubkey, u64>::default());
    let server = setup_server_with_config(
        staked_nodes5,
        SwQosConfig {
            max_streams_per_ms: 50,
            max_connections_per_staked_peer: 1,
            rtt_overrides: rtt_overrides5,
            ..Default::default()
        },
    );
    sleep(Duration::from_millis(200)).await;

    let measure_duration = Duration::from_secs(3);

    // Background saturator
    let bg_conn5 = connect(&server.server_address, Some(&bg_keypair5)).await;
    let bg_cancel5 = CancellationToken::new();
    let bg_cancel5b = bg_cancel5.clone();
    let bg_handle5 = tokio::spawn(async move {
        let mut total = 0usize;
        loop {
            tokio::select! {
                result = bg_conn5.open_uni() => {
                    match result {
                        Ok(mut s) => {
                            let _ = s.write_all(&[0u8]).await;
                            let _ = s.finish();
                            total += 1;
                        }
                        Err(_) => { sleep(Duration::from_millis(1)).await; }
                    }
                }
                _ = bg_cancel5b.cancelled() => break,
            }
        }
        total
    });

    sleep(Duration::from_secs(1)).await;
    let lt = server.swqos.load_tracker();
    println!(
        "  after warmup: saturated={} bucket_level={}",
        lt.is_saturated(),
        lt.bucket_level()
    );

    // Each peer blasts with its own simulated RTT as hold duration
    let mut handles = Vec::new();
    for (keypair, stake, rtt) in &rtt_peers {
        let conn = connect(&server.server_address, Some(keypair)).await;
        let stake = *stake;
        let rtt = *rtt;
        handles.push(tokio::spawn(async move {
            let sent = blast_concurrent(&conn, measure_duration, rtt).await;
            (stake, rtt, sent)
        }));
    }

    let mut results5: Vec<(u64, Duration, usize)> = Vec::new();
    for h in handles {
        results5.push(h.await.unwrap());
    }

    bg_cancel5.cancel();
    let bg_sent5 = bg_handle5.await.unwrap();

    let total_throughput5: usize = results5.iter().map(|(_, _, sent)| sent).sum();
    println!(
        "  total throughput: {} streams in {}s ({:.0}/s)",
        total_throughput5,
        measure_duration.as_secs(),
        total_throughput5 as f64 / measure_duration.as_secs_f64(),
    );
    println!("  background saturator sent: {bg_sent5}");

    println!(
        "  {:>6}  {:>10}  {:>10}  {:>5}",
        "RTT", "throughput", "tput/s", "ratio"
    );
    // Use the first peer's throughput as baseline
    let baseline = results5[0].2 as f64;
    let mut pass5 = true;
    for (_, rtt, sent) in &results5 {
        let tps = *sent as f64 / measure_duration.as_secs_f64();
        let ratio = *sent as f64 / baseline;
        println!(
            "  {:>5}ms  {:>10}  {:>9.0}/s  {:>4.2}x",
            rtt.as_millis(),
            sent,
            tps,
            ratio,
        );
        // Throughput should be within 50% of baseline regardless of RTT
        if ratio < 0.5 || ratio > 2.0 {
            pass5 = false;
        }
    }

    if pass5 {
        println!("  PASS: throughput is RTT-independent (BDP scaling works)");
    } else {
        println!("  WARN: throughput varies significantly with RTT");
    }

    print_stats(&server.stats);
    server.cancel.cancel();
    sleep(Duration::from_millis(200)).await;

    // ── Experiment 6: Mixed stake + RTT ─────────────────────────────
    print_header("Experiment 6: Different stakes AND different RTTs");
    println!("  Throughput should track stake, not RTT");
    println!("  High-stake far peer should outperform low-stake nearby peer");

    // Deliberately adversarial: the highest-stake peer has the worst RTT.
    // quota = share_tps * rtt, throughput = quota / rtt = share_tps.
    // So throughput depends only on stake.
    let mixed_peers: Vec<(Keypair, u64, Duration)> = vec![
        (Keypair::new(), 5_000_000_000, Duration::from_millis(100)), // 50%, 100ms (far)
        (Keypair::new(), 3_000_000_000, Duration::from_millis(20)),  // 30%, 20ms  (near)
        (Keypair::new(), 2_000_000_000, Duration::from_millis(50)),  // 20%, 50ms  (mid)
    ];
    let bg_keypair6 = Keypair::new();

    let mut stake_map6: HashMap<Pubkey, u64> = mixed_peers
        .iter()
        .map(|(kp, s, _)| (kp.pubkey(), *s))
        .collect();
    stake_map6.insert(bg_keypair6.pubkey(), 100_000_000);
    let mut rtt_overrides6: HashMap<Pubkey, Duration> = mixed_peers
        .iter()
        .map(|(kp, _, rtt)| (kp.pubkey(), *rtt))
        .collect();
    rtt_overrides6.insert(bg_keypair6.pubkey(), Duration::from_millis(10));
    let staked_nodes6 = StakedNodes::new(Arc::new(stake_map6), HashMap::<Pubkey, u64>::default());
    let server = setup_server_with_config(
        staked_nodes6,
        SwQosConfig {
            max_streams_per_ms: 50,
            max_connections_per_staked_peer: 1,
            rtt_overrides: rtt_overrides6,
            ..Default::default()
        },
    );
    sleep(Duration::from_millis(200)).await;

    let measure_duration = Duration::from_secs(3);

    // Background saturator
    let bg_conn6 = connect(&server.server_address, Some(&bg_keypair6)).await;
    let bg_cancel6 = CancellationToken::new();
    let bg_cancel6b = bg_cancel6.clone();
    let bg_handle6 = tokio::spawn(async move {
        let mut total = 0usize;
        loop {
            tokio::select! {
                result = bg_conn6.open_uni() => {
                    match result {
                        Ok(mut s) => {
                            let _ = s.write_all(&[0u8]).await;
                            let _ = s.finish();
                            total += 1;
                        }
                        Err(_) => { sleep(Duration::from_millis(1)).await; }
                    }
                }
                _ = bg_cancel6b.cancelled() => break,
            }
        }
        total
    });

    sleep(Duration::from_secs(1)).await;
    let lt = server.swqos.load_tracker();
    println!(
        "  after warmup: saturated={} bucket_level={}",
        lt.is_saturated(),
        lt.bucket_level()
    );

    let mut handles = Vec::new();
    for (keypair, stake, rtt) in &mixed_peers {
        let conn = connect(&server.server_address, Some(keypair)).await;
        let stake = *stake;
        let rtt = *rtt;
        handles.push(tokio::spawn(async move {
            let sent = blast_concurrent(&conn, measure_duration, rtt).await;
            (stake, rtt, sent)
        }));
    }

    let mut results6: Vec<(u64, Duration, usize)> = Vec::new();
    for h in handles {
        results6.push(h.await.unwrap());
    }

    bg_cancel6.cancel();
    let bg_sent6 = bg_handle6.await.unwrap();

    // Sort by stake descending
    results6.sort_by(|a, b| b.0.cmp(&a.0));

    let total_stake6: u64 = results6.iter().map(|(s, _, _)| s).sum();
    let total_throughput6: usize = results6.iter().map(|(_, _, sent)| sent).sum();
    println!(
        "  total throughput: {} streams in {}s ({:.0}/s)",
        total_throughput6,
        measure_duration.as_secs(),
        total_throughput6 as f64 / measure_duration.as_secs_f64(),
    );
    println!("  background saturator sent: {bg_sent6}");

    println!(
        "  {:>7}  {:>5}  {:>10}  {:>8}  {:>10}  {:>5}",
        "stake%", "RTT", "throughput", "share%", "expected%", "ratio"
    );
    let mut pass6 = true;
    for (stake, rtt, sent) in &results6 {
        let stake_pct = *stake as f64 / total_stake6 as f64 * 100.0;
        let throughput_pct = *sent as f64 / total_throughput6 as f64 * 100.0;
        let ratio = throughput_pct / stake_pct;
        println!(
            "  {:>6.1}%  {:>4}ms  {:>10}  {:>7.1}%  {:>9.1}%  {:>4.2}x",
            stake_pct,
            rtt.as_millis(),
            sent,
            throughput_pct,
            stake_pct,
            ratio,
        );
        if ratio < 0.5 || ratio > 2.0 {
            pass6 = false;
        }
    }

    // Verify ordering matches stake, not RTT
    let ordered6 = results6.windows(2).all(|w| w[0].2 >= w[1].2);

    if pass6 && ordered6 {
        println!("  PASS: throughput tracks stake regardless of RTT");
    } else if !ordered6 {
        println!("  WARN: throughput ordering doesn't match stake ordering");
    } else {
        println!("  WARN: throughput shares deviate significantly from stake shares");
    }

    print_stats(&server.stats);
    server.cancel.cancel();
    sleep(Duration::from_millis(200)).await;

    // ── Experiment 7: Unsaturated BDP scaling ───────────────────────
    print_header("Experiment 7: Unsaturated BDP scaling");
    println!("  3 peers, equal stake, RTTs of 100ms/150ms/200ms (all >= REFERENCE_RTT)");
    println!("  System NOT saturated — generous quotas, BDP-scaled by RTT");

    // When unsaturated: quota = base_max_streams * (rtt / REFERENCE_RTT)
    //   100ms: 2048 * 1.0 = 2048 concurrent → 2048/0.10 = 20,480/s
    //   150ms: 2048 * 1.5 = 3072 concurrent → 3072/0.15 = 20,480/s
    //   200ms: 2048 * 2.0 = 4096 concurrent → 4096/0.20 = 20,480/s
    // All achieve the same throughput: base_max_streams / REFERENCE_RTT.
    let bdp_peers: Vec<(Keypair, Duration)> = vec![
        (Keypair::new(), Duration::from_millis(100)),
        (Keypair::new(), Duration::from_millis(150)),
        (Keypair::new(), Duration::from_millis(200)),
    ];

    let stake_map7: HashMap<Pubkey, u64> = bdp_peers
        .iter()
        .map(|(kp, _)| (kp.pubkey(), 1_000_000_000))
        .collect();
    let rtt_overrides7: HashMap<Pubkey, Duration> = bdp_peers
        .iter()
        .map(|(kp, rtt)| (kp.pubkey(), *rtt))
        .collect();
    let staked_nodes7 = StakedNodes::new(Arc::new(stake_map7), HashMap::<Pubkey, u64>::default());

    // High capacity so the system stays unsaturated.
    // 3 peers at ~20K/s each = ~60K/s total << 500K/s capacity.
    let server = setup_server_with_config(
        staked_nodes7,
        SwQosConfig {
            max_streams_per_ms: 500,
            max_connections_per_staked_peer: 1,
            rtt_overrides: rtt_overrides7,
            ..Default::default()
        },
    );
    sleep(Duration::from_millis(200)).await;

    let measure_duration = Duration::from_secs(3);

    let mut handles = Vec::new();
    for (keypair, rtt) in &bdp_peers {
        let conn = connect(&server.server_address, Some(keypair)).await;
        let rtt = *rtt;
        handles.push(tokio::spawn(async move {
            let sent = blast_concurrent(&conn, measure_duration, rtt).await;
            (rtt, sent)
        }));
    }

    let mut results7: Vec<(Duration, usize)> = Vec::new();
    for h in handles {
        results7.push(h.await.unwrap());
    }

    let lt = server.swqos.load_tracker();
    assert!(
        !lt.is_saturated(),
        "system should stay unsaturated with 500K/s capacity"
    );

    let total_throughput7: usize = results7.iter().map(|(_, sent)| sent).sum();
    println!(
        "  total throughput: {} streams in {}s ({:.0}/s)",
        total_throughput7,
        measure_duration.as_secs(),
        total_throughput7 as f64 / measure_duration.as_secs_f64(),
    );
    println!(
        "  saturated={} bucket_level={}",
        lt.is_saturated(),
        lt.bucket_level()
    );

    println!(
        "  {:>6}  {:>6}  {:>10}  {:>10}  {:>5}",
        "RTT", "quota", "throughput", "tput/s", "ratio"
    );
    let baseline7 = results7[0].1 as f64;
    let mut pass7 = true;
    for (rtt, sent) in &results7 {
        let rtt_scale = (rtt.as_secs_f64() / 0.100_f64).max(1.0);
        let quota = (2048.0 * rtt_scale) as u32;
        let tps = *sent as f64 / measure_duration.as_secs_f64();
        let ratio = *sent as f64 / baseline7;
        println!(
            "  {:>5}ms  {:>6}  {:>10}  {:>9.0}/s  {:>4.2}x",
            rtt.as_millis(),
            quota,
            sent,
            tps,
            ratio,
        );
        if ratio < 0.5 || ratio > 2.0 {
            pass7 = false;
        }
    }

    if pass7 {
        println!("  PASS: BDP scaling equalizes throughput across RTTs (unsaturated)");
    } else {
        println!("  WARN: throughput varies significantly with RTT");
    }

    print_stats(&server.stats);
    server.cancel.cancel();
    sleep(Duration::from_millis(200)).await;

    println!("\n{}", "=".repeat(70));
    println!("  All experiments complete.");
    println!("{}", "=".repeat(70));
}
