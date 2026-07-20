//! End-to-end convergence tests: several in-process nodes with real QUIC
//! datagram endpoints on localhost, each running the full clock-sync service
//! against a skewed local clock.

#![cfg(feature = "agave-unstable-api")]
#![allow(clippy::arithmetic_side_effects)]

use {
    agave_clock_sync::{
        CLOCK_SYNC_ALPN,
        clock::SyncedClock,
        protocol,
        service::{ClockSyncService, ServiceTiming, SharedStakes},
    },
    agave_votor_transport::endpoint::{BanCommand, ExitSignals, QuicDatagramEndpoint},
    arc_swap::ArcSwap,
    solana_keypair::Keypair,
    solana_net_utils::sockets::bind_to_localhost_unique,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    std::{
        collections::HashMap,
        net::SocketAddr,
        sync::{Arc, atomic::AtomicBool},
        time::Duration,
    },
    tokio::{
        runtime::Runtime,
        sync::{mpsc, watch},
    },
    tokio_util::sync::CancellationToken,
};

/// Fast rounds so the test covers many of them: T = 100ms, W = 50ms, S = 5ms.
fn test_timing() -> ServiceTiming {
    ServiceTiming {
        period: Duration::from_millis(100),
        window: Duration::from_millis(50),
        absorption_half_width: Duration::from_millis(5),
    }
}

/// Generous per-peer rate limit: test rounds are 10x faster than production.
const TEST_RATE_LIMIT_PPS: usize = 100;

struct Node {
    pubkey: Pubkey,
    addr: SocketAddr,
    clock: Arc<SyncedClock>,
    endpoint: QuicDatagramEndpoint,
    service: Option<ClockSyncService>,
    peer_list_sender: watch::Sender<Arc<HashMap<Pubkey, Option<SocketAddr>>>>,
    stakes: SharedStakes,
    exit: Arc<AtomicBool>,
    _ban_sender: mpsc::Sender<BanCommand>,
}

impl Node {
    /// Bring up transport + service with the given clock skew. The service
    /// starts pulsing immediately; peers are added via `set_peers`.
    fn start(runtime: &Runtime, skew_ns: i64, run_service: bool) -> Self {
        let keypair = Keypair::new();
        let pubkey = keypair.pubkey();
        let inbound = bind_to_localhost_unique().expect("bind inbound");
        let addr = inbound.local_addr().expect("inbound addr");
        let outbound = bind_to_localhost_unique().expect("bind outbound");
        let exit = Arc::new(AtomicBool::new(false));
        let (ingress_sender, ingress_receiver) = crossbeam_channel::bounded(4096);
        let (ban_sender, ban_receiver) = mpsc::channel(8);
        let (peer_list_sender, peer_list_receiver) = watch::channel(Arc::new(HashMap::new()));
        let endpoint = QuicDatagramEndpoint::spawn(
            runtime.handle(),
            &keypair,
            CLOCK_SYNC_ALPN,
            vec![inbound],
            outbound,
            ingress_sender,
            peer_list_receiver,
            ban_receiver,
            TEST_RATE_LIMIT_PPS,
            ExitSignals::new(exit.clone(), CancellationToken::new()),
        )
        .expect("spawn endpoint");
        let clock = Arc::new(SyncedClock::with_skew(skew_ns));
        let stakes: SharedStakes = Arc::new(ArcSwap::from_pointee(HashMap::new()));
        let service = run_service.then(|| {
            ClockSyncService::new(
                pubkey,
                test_timing(),
                clock.clone(),
                endpoint.egress.clone(),
                ingress_receiver,
                stakes.clone(),
                exit.clone(),
            )
        });
        Self {
            pubkey,
            addr,
            clock,
            endpoint,
            service,
            peer_list_sender,
            stakes,
            exit,
            _ban_sender: ban_sender,
        }
    }

    fn set_peers(
        &self,
        peers: &HashMap<Pubkey, Option<SocketAddr>>,
        stakes: &HashMap<Pubkey, u64>,
    ) {
        self.peer_list_sender.send_replace(Arc::new(peers.clone()));
        self.stakes.store(Arc::new(stakes.clone()));
    }

    fn shutdown(mut self, runtime: &Runtime) {
        self.exit.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(service) = self.service.take() {
            service.join().expect("service join");
        }
        runtime
            .block_on(self.endpoint.shutdown())
            .expect("endpoint shutdown");
    }
}

fn full_mesh(nodes: &[Node], stakes: &[u64]) {
    let peers: HashMap<Pubkey, Option<SocketAddr>> = nodes
        .iter()
        .map(|node| (node.pubkey, Some(node.addr)))
        .collect();
    let stake_map: HashMap<Pubkey, u64> = nodes
        .iter()
        .zip(stakes)
        .map(|(node, stake)| (node.pubkey, *stake))
        .collect();
    for node in nodes {
        node.set_peers(&peers, &stake_map);
    }
}

fn max_pairwise_skew_ns(nodes: &[Node]) -> i64 {
    let readings: Vec<i64> = nodes.iter().map(|node| node.clock.now_ns()).collect();
    let min = readings.iter().min().copied().unwrap();
    let max = readings.iter().max().copied().unwrap();
    max - min
}

fn make_runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("tokio runtime")
}

#[test]
fn four_honest_nodes_converge() {
    agave_logger::setup();
    let runtime = make_runtime();
    // Initial skews well inside the 50ms acceptance window.
    let skews_ns = [0i64, 30_000_000, -20_000_000, 10_000_000];
    let nodes: Vec<Node> = skews_ns
        .iter()
        .map(|skew| Node::start(&runtime, *skew, true))
        .collect();
    full_mesh(&nodes, &[1, 1, 1, 1]);

    let initial = max_pairwise_skew_ns(&nodes);
    assert!(
        initial > 20_000_000,
        "test setup should start with real skew, got {initial}ns"
    );

    // ~40 rounds: connections come up in the first few, then the fine loop
    // pulls everyone together.
    std::thread::sleep(Duration::from_secs(4));

    let along_the_way = max_pairwise_skew_ns(&nodes);
    std::thread::sleep(Duration::from_secs(1));
    let settled = max_pairwise_skew_ns(&nodes);
    assert!(
        settled < 5_000_000,
        "clocks should agree within 5ms after convergence, got {settled}ns (was {along_the_way}ns \
         one second earlier, {initial}ns at start)"
    );

    for node in nodes {
        node.shutdown(&runtime);
    }
}

#[test]
fn byzantine_minority_cannot_prevent_convergence() {
    agave_logger::setup();
    let runtime = make_runtime();
    // Three honest nodes and one adversary holding just under a third of
    // the stake (4/13). The adversary blasts garbage and wrong-round pulses.
    let skews_ns = [0i64, 25_000_000, -25_000_000];
    let mut nodes: Vec<Node> = skews_ns
        .iter()
        .map(|skew| Node::start(&runtime, *skew, true))
        .collect();
    let adversary = Node::start(&runtime, 0, false);
    let adversary_egress = adversary.endpoint.egress.clone();
    nodes.push(adversary);
    full_mesh(&nodes, &[3, 3, 3, 4]);
    let adversary = nodes.pop().unwrap();

    // Adversarial sender: garbage bytes and absurd round tags, as fast as
    // the rate limit allows.
    let attack_exit = Arc::new(AtomicBool::new(false));
    let attacker = {
        let exit = attack_exit.clone();
        std::thread::spawn(move || {
            let mut round = 0u64;
            while !exit.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = adversary_egress.try_send(bytes::Bytes::from_static(b"garbage"));
                let _ = adversary_egress.try_send(protocol::encode_pulse(round, i64::MIN));
                let _ =
                    adversary_egress.try_send(protocol::encode_pulse(u64::MAX - round, 999_999));
                round += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };

    std::thread::sleep(Duration::from_secs(5));

    let settled = max_pairwise_skew_ns(&nodes);
    assert!(
        settled < 5_000_000,
        "honest clocks should agree within 5ms despite the adversary, got {settled}ns"
    );

    attack_exit.store(true, std::sync::atomic::Ordering::Relaxed);
    attacker.join().expect("attacker thread");
    adversary.shutdown(&runtime);
    for node in nodes {
        node.shutdown(&runtime);
    }
}
