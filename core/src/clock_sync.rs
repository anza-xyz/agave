//! Wires the clock-sync protocol (`agave-clock-sync`) into the validator:
//! spawns its QUIC datagram endpoint, the service thread, and a peer-list
//! updater that publishes the current-epoch staked set (addresses for the
//! transport, stakes for the fault-tolerant midpoint).
//!
//! Experimental and opt-in (`--experimental-clock-sync`); shadow mode only.

use {
    crate::admin_rpc_post_init::{KeyUpdaterType, KeyUpdaters},
    agave_clock_sync::{
        CLOCK_SYNC_ALPN, CLOCK_SYNC_RATE_LIMIT_PPS,
        clock::SyncedClock,
        service::{ClockSyncService, SharedStakes},
    },
    agave_votor_transport::{
        PeerListSender,
        endpoint::{BanCommand, ExitSignals, QuicDatagramEndpoint},
    },
    arc_swap::ArcSwap,
    log::error,
    solana_gossip::cluster_info::ClusterInfo,
    solana_pubkey::Pubkey,
    solana_runtime::bank_forks::BankForks,
    std::{
        collections::HashMap,
        net::{SocketAddr, UdpSocket},
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, Ordering},
        },
        thread::{Builder, JoinHandle},
        time::Duration,
    },
    tokio::{
        runtime::{Handle, Runtime as TokioRuntime},
        sync::{mpsc, watch},
    },
    tokio_util::sync::CancellationToken,
};

/// How often the peer list and stakes are refreshed from the root bank and
/// gossip. Matches the votor peer-list cadence.
const PEER_LIST_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// Inbound datagram channel capacity: at 1 pulse/second/peer this holds
/// several rounds from the whole cluster.
const INGRESS_CHANNEL_CAP: usize = 8192;

pub struct ClockSyncHandles {
    endpoint: QuicDatagramEndpoint,
    // `None` when running on an ambient runtime (test-validator).
    runtime: Option<TokioRuntime>,
    service: ClockSyncService,
    updater: JoinHandle<()>,
    /// The synchronized virtual clock. Shadow mode: exposed for metrics and
    /// future consumers, read by nothing consensus-facing.
    pub clock: Arc<SyncedClock>,
    /// The endpoint exits if the ban channel closes; hold the (unused)
    /// sender for the endpoint's lifetime.
    _ban_sender: mpsc::Sender<BanCommand>,
}

impl ClockSyncHandles {
    pub fn join(mut self) -> std::thread::Result<()> {
        if let Some(rt) = self.runtime.take() {
            rt.block_on(self.endpoint.shutdown())
                .expect("clock-sync transport should exit cleanly");
            rt.shutdown_background();
        }
        self.service.join()?;
        self.updater.join()
    }
}

pub fn start_clock_sync(
    cluster_info: Arc<ClusterInfo>,
    bank_forks: Arc<RwLock<BankForks>>,
    clock_sync_socket: UdpSocket,
    clock_sync_client_socket: UdpSocket,
    key_notifiers: &Arc<RwLock<KeyUpdaters>>,
    exit: Arc<AtomicBool>,
    cancel: CancellationToken,
) -> Result<ClockSyncHandles, String> {
    let (runtime, rt_handle) = match Handle::try_current() {
        Ok(handle) => (None, handle),
        Err(_) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                // 1 core for TLS/handshake work, 1 for connections and egress:
                // clock-sync traffic is 1 datagram/second/peer.
                .worker_threads(2)
                .thread_name("solClkSyncRt")
                .build()
                .map_err(|err| format!("clock-sync runtime: {err:?}"))?;
            let handle = rt.handle().clone();
            (Some(rt), handle)
        }
    };

    let (ingress_sender, ingress_receiver) = crossbeam_channel::bounded(INGRESS_CHANNEL_CAP);
    let (ban_sender, ban_receiver) = mpsc::channel(8);
    let stakes: SharedStakes = Arc::new(ArcSwap::from_pointee(HashMap::new()));
    let (peer_list_sender, peer_list_receiver) = watch::channel(Arc::new(HashMap::new()));
    // Seed the peer list and stakes immediately so inbound connections from
    // staked peers are admitted during ledger replay.
    refresh_peer_list(&cluster_info, &bank_forks, &peer_list_sender, &stakes);

    let endpoint = QuicDatagramEndpoint::spawn(
        &rt_handle,
        &cluster_info.keypair(),
        CLOCK_SYNC_ALPN,
        vec![clock_sync_socket],
        clock_sync_client_socket,
        ingress_sender,
        peer_list_receiver,
        ban_receiver,
        CLOCK_SYNC_RATE_LIMIT_PPS,
        ExitSignals::new(exit.clone(), cancel),
    )
    .map_err(|err| format!("clock-sync endpoint: {err:?}"))?;
    key_notifiers
        .write()
        .unwrap()
        .add(KeyUpdaterType::ClockSync, endpoint.key_updater.clone());

    let clock = Arc::new(SyncedClock::new());
    let service = ClockSyncService::new(
        cluster_info.id(),
        clock.clone(),
        endpoint.egress.clone(),
        ingress_receiver,
        stakes.clone(),
        exit.clone(),
    );

    let updater = Builder::new()
        .name("solClkSyncPeers".to_string())
        .spawn(move || {
            while !exit.load(Ordering::Relaxed) {
                refresh_peer_list(&cluster_info, &bank_forks, &peer_list_sender, &stakes);
                std::thread::sleep(PEER_LIST_REFRESH_INTERVAL);
            }
        })
        .map_err(|err| format!("clock-sync peer updater: {err:?}"))?;

    Ok(ClockSyncHandles {
        endpoint,
        runtime,
        service,
        updater,
        clock,
        _ban_sender: ban_sender,
    })
}

/// Publish the current-epoch staked set: pubkey -> clock-sync socket for the
/// transport's admission/connection control, pubkey -> stake for the
/// fault-tolerant midpoint. An unstaked node publishes empty maps and
/// free-runs (votor semantics; no epoch-boundary merging, the protocol
/// tolerates brief churn).
fn refresh_peer_list(
    cluster_info: &ClusterInfo,
    bank_forks: &RwLock<BankForks>,
    peer_list_sender: &PeerListSender,
    stakes: &SharedStakes,
) {
    let root_bank = bank_forks.read().unwrap().root_bank();
    let Some(staked_nodes) = root_bank.epoch_staked_nodes(root_bank.epoch()) else {
        error!(
            "clock-sync can not update the peer list: epoch_staked_nodes not available for epoch \
             {}.",
            root_bank.epoch()
        );
        return;
    };
    if !staked_nodes.contains_key(&cluster_info.id()) {
        stakes.store(Arc::new(HashMap::new()));
        peer_list_sender.send_replace(Arc::new(HashMap::new()));
        return;
    }
    let peer_list: HashMap<Pubkey, Option<SocketAddr>> = staked_nodes
        .keys()
        .map(|pubkey| {
            let socket = cluster_info
                .lookup_contact_info(pubkey, |node| node.clock_sync())
                .flatten();
            (*pubkey, socket)
        })
        .collect();
    stakes.store(Arc::new(staked_nodes.as_ref().clone()));
    peer_list_sender.send_replace(Arc::new(peer_list));
}
