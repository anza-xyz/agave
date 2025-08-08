use {
    crate::{
        consensus::tower_storage::{SavedTowerVersions, TowerStorage},
        mock_alpenglow_consensus::MockAlpenglowConsensus,
        next_leader::upcoming_leader_tpu_vote_sockets,
        tvu::VoteClientOption,
    },
    async_trait::async_trait,
    bincode::serialize,
    crossbeam_channel::Receiver,
    solana_client::connection_cache::ConnectionCache,
    solana_clock::{
        Slot, FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET, NUM_CONSECUTIVE_LEADER_SLOTS,
    },
    solana_connection_cache::client_connection::ClientConnection,
    solana_gossip::{{cluster_info::ClusterInfo, epoch_specs::EpochSpecs}, contact_info::Protocol},
    solana_keypair::Keypair,
    solana_measure::measure::Measure,
    solana_poh::poh_recorder::PohRecorder,
    solana_runtime::bank_forks::BankForks,
    solana_pubkey::Pubkey,
    solana_quic_definitions::NotifyKeyUpdate,
    solana_tpu_client_next::{
        connection_workers_scheduler::{
            BindTarget, ConnectionWorkersSchedulerConfig, Fanout, StakeIdentity,
        },
        leader_updater::LeaderUpdater,
        transaction_batch::TransactionBatch,
        ConnectionWorkersScheduler,
    },
    solana_transaction::Transaction,
    solana_transaction_error::TransportError,
    std::{
        collections::HashMap,
        iter::once,
        net::{{SocketAddr, UdpSocket}, UdpSocket},
        sync::{Arc, Mutex, RwLock},
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
    thiserror::Error,
    tokio::sync::{mpsc, watch},
    tokio_util::sync::CancellationToken,
};

pub enum VoteOp {
    PushVote {
        tx: Transaction,
        tower_slots: Vec<Slot>,
        saved_tower: SavedTowerVersions,
    },
    RefreshVote {
        tx: Transaction,
        last_voted_slot: Slot,
    },
}

impl VoteOp {
    fn tx(&self) -> &Transaction {
        match self {
            VoteOp::PushVote { tx, .. } => tx,
            VoteOp::RefreshVote { tx, .. } => tx,
        }
    }
}

#[derive(Clone)]
pub struct SendTransactionServiceLeaderUpdater<T: TpuInfoWithSendStatic> {
    leader_info_provider: CurrentLeaderInfo<T>,
    my_tpu_address: SocketAddr,
}

#[async_trait]
impl<T> LeaderUpdater for SendTransactionServiceLeaderUpdater<T>
where
    T: TpuInfoWithSendStatic,
{
    fn next_leaders(&mut self, lookahead_leaders: usize) -> Vec<SocketAddr> {
        self.leader_info_provider
            .get_leader_info()
            .map(|leader_info| {
                leader_info
                    .get_not_unique_leader_tpus(lookahead_leaders as u64, Protocol::QUIC)
                    .into_iter()
                    .cloned()
                    .collect::<Vec<SocketAddr>>()
            })
            .filter(|addresses| !addresses.is_empty())
            .unwrap_or_else(|| vec![self.my_tpu_address])
    }
    async fn stop(&mut self) {}
}

/// A trait to abstract out the leader estimation for the
/// SendTransactionService.
pub trait TpuInfo {
    fn refresh_recent_peers(&mut self);

    /// Takes `max_count` which specifies how many leaders per
    /// `NUM_CONSECUTIVE_LEADER_SLOTS` we want to receive and returns *unique*
    /// TPU socket addresses for these leaders.
    ///
    /// For example, if leader schedule was `[L1, L1, L1, L1, L2, L2, L2, L2,
    /// L1, ...]` it will return `[L1, L2]` (the last L1 will be not added to
    /// the result).
    fn get_leader_tpus(&self, max_count: u64, protocol: Protocol) -> Vec<&SocketAddr>;

    /// Takes `max_count` which specifies how many leaders per
    /// `NUM_CONSECUTIVE_LEADER_SLOTS` we want to receive and returns TPU socket
    /// addresses for these leaders.
    ///
    /// For example, if leader schedule was `[L1, L1, L1, L1, L2, L2, L2, L2,
    /// L1, ...]` it will return `[L1, L2, L1]`.
    fn get_not_unique_leader_tpus(&self, max_count: u64, protocol: Protocol) -> Vec<&SocketAddr>;
}

// Alias trait to shorten function definitions.
pub trait TpuInfoWithSendStatic: TpuInfo + std::marker::Send + 'static {}
impl<T> TpuInfoWithSendStatic for T where T: TpuInfo + std::marker::Send + 'static {}

pub trait VoteClient {
    fn send_transactions_in_batch(&self, wire_transactions: Vec<Vec<u8>>);

    #[cfg(any(test, feature = "dev-context-only-utils"))]
    fn protocol(&self) -> Protocol;
}

/// The leader info refresh rate.
pub const LEADER_INFO_REFRESH_RATE_MS: u64 = 1000;

/// A struct responsible for holding up-to-date leader information
/// used for sending transactions.
#[derive(Clone)]
pub struct CurrentLeaderInfo<T>
where
    T: TpuInfoWithSendStatic,
{
    /// The last time the leader info was refreshed
    last_leader_refresh: Option<Instant>,

    /// The leader info
    leader_info: Option<T>,

    /// How often to refresh the leader info
    refresh_rate: Duration,
}

impl<T> CurrentLeaderInfo<T>
where
    T: TpuInfoWithSendStatic,
{
    /// Get the leader info, refresh if expired
    pub fn get_leader_info(&mut self) -> Option<&T> {
        if let Some(leader_info) = self.leader_info.as_mut() {
            let now = Instant::now();
            let need_refresh = self
                .last_leader_refresh
                .map(|last| now.duration_since(last) >= self.refresh_rate)
                .unwrap_or(true);

            if need_refresh {
                leader_info.refresh_recent_peers();
                self.last_leader_refresh = Some(now);
            }
        }
        self.leader_info.as_ref()
    }

    pub fn new(leader_info: Option<T>) -> Self {
        Self {
            last_leader_refresh: None,
            leader_info,
            refresh_rate: Duration::from_millis(LEADER_INFO_REFRESH_RATE_MS),
        }
    }
}

pub struct ConnectionCacheClient<T: TpuInfoWithSendStatic> {
    connection_cache: Arc<ConnectionCache>,
    tpu_address: SocketAddr,
    leader_info_provider: Arc<Mutex<CurrentLeaderInfo<T>>>,
    leader_forward_count: u64,
}

// Manual implementation of Clone without requiring T to be Clone
impl<T> Clone for ConnectionCacheClient<T>
where
    T: TpuInfoWithSendStatic,
{
    fn clone(&self) -> Self {
        Self {
            connection_cache: Arc::clone(&self.connection_cache),
            tpu_address: self.tpu_address,
            leader_info_provider: Arc::clone(&self.leader_info_provider),
            leader_forward_count: self.leader_forward_count,
        }
    }
}

impl<T> ConnectionCacheClient<T>
where
    T: TpuInfoWithSendStatic,
{
    pub fn new(
        connection_cache: Arc<ConnectionCache>,
        tpu_address: SocketAddr,
        leader_info: Option<T>,
        leader_forward_count: u64,
    ) -> Self {
        let leader_info_provider = Arc::new(Mutex::new(CurrentLeaderInfo::new(leader_info)));
        Self {
            connection_cache,
            tpu_address,
            leader_info_provider,
            leader_forward_count,
        }
    }

    fn get_tpu_addresses<'a>(&'a self, leader_info: Option<&'a T>) -> Vec<&'a SocketAddr> {
        leader_info
            .map(|leader_info| {
                leader_info
                    .get_leader_tpus(self.leader_forward_count, self.connection_cache.protocol())
            })
            .filter(|addresses| !addresses.is_empty())
            .unwrap_or_else(|| vec![&self.tpu_address])
    }

    fn send_transactions(&self, peer: &SocketAddr, wire_transactions: Vec<Vec<u8>>) {
        let conn = self.connection_cache.get_connection(peer);
        let result = conn.send_data_batch_async(wire_transactions);

        if let Err(err) = result {
            warn!(
                "Failed to send transaction transaction to {}: {:?}",
                self.tpu_address, err
            );
        }
    }
}

impl<T> VoteClient for ConnectionCacheClient<T>
where
    T: TpuInfoWithSendStatic,
{
    fn send_transactions_in_batch(&self, wire_transactions: Vec<Vec<u8>>) {
        let mut leader_info_provider = self.leader_info_provider.lock().unwrap();
        let leader_info = leader_info_provider.get_leader_info();
        let leader_addresses = self.get_tpu_addresses(leader_info);

        for address in &leader_addresses {
            self.send_transactions(address, wire_transactions.clone());
        }
    }

    #[cfg(any(test, feature = "dev-context-only-utils"))]
    fn protocol(&self) -> Protocol {
        self.connection_cache.protocol()
    }
}

#[derive(Clone)]
struct TpuClientNextClient {
    runtime_handle: tokio::runtime::Handle,
    sender: mpsc::Sender<TransactionBatch>,
    update_certificate_sender: watch::Sender<Option<StakeIdentity>>,
}

const METRICS_REPORTING_INTERVAL: Duration = Duration::from_secs(5);

impl TpuClientNextClient {
    fn new<T>(
        runtime_handle: tokio::runtime::Handle,
        my_tpu_address: SocketAddr,
        leader_info: Option<T>,
        stake_identity: Option<&Keypair>,
        bind_socket: UdpSocket,
        cancel: CancellationToken,
    ) -> Self
    where
        T: TpuInfoWithSendStatic + Clone,
    {
        // For now use large channel, the more suitable size to be found later.
        let (sender, receiver) = mpsc::channel(128);
        let leader_info_provider = CurrentLeaderInfo::new(leader_info);

        let leader_updater: SendTransactionServiceLeaderUpdater<T> =
            SendTransactionServiceLeaderUpdater {
                leader_info_provider,
                my_tpu_address,
            };

        let config = Self::create_config(bind_socket, stake_identity);
        let (update_certificate_sender, update_certificate_receiver) = watch::channel(None);
        let scheduler: ConnectionWorkersScheduler = ConnectionWorkersScheduler::new(
            Box::new(leader_updater),
            receiver,
            update_certificate_receiver,
            cancel.clone(),
        );
        // leaking handle to this task, as it will run until the cancel signal is received
        runtime_handle.spawn(scheduler.get_stats().report_to_influxdb(
            "vote-client",
            METRICS_REPORTING_INTERVAL,
            cancel.clone(),
        ));
        let _handle = runtime_handle.spawn(scheduler.run(config));
        Self {
            runtime_handle,
            sender,
            update_certificate_sender,
        }
    }

    fn create_config(
        bind_socket: UdpSocket,
        stake_identity: Option<&Keypair>,
    ) -> ConnectionWorkersSchedulerConfig {
        ConnectionWorkersSchedulerConfig {
            bind: BindTarget::Socket(bind_socket),
            stake_identity: stake_identity.map(StakeIdentity::new),
            // Cache size of 128 covers all nodes above the P90 slot count threshold,
            // which together account for ~75% of total slots in the epoch.
            num_connections: 128,
            skip_check_transaction_age: true,
            worker_channel_size: 2,
            max_reconnect_attempts: 4,
            // Send to the next leader only, but verify that connections exist
            // for the leaders of the next `4 * NUM_CONSECUTIVE_SLOTS`.
            leaders_fanout: Fanout {
                send: 1,
                connect: 4,
            },
        }
    }
}

impl NotifyKeyUpdate for TpuClientNextClient {
    fn update_key(&self, identity: &Keypair) -> Result<(), Box<dyn std::error::Error>> {
        let stake_identity = StakeIdentity::new(identity);
        self.update_certificate_sender
            .send(Some(stake_identity))
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
    }
}

impl VoteClient for TpuClientNextClient {
    fn send_transactions_in_batch(&self, wire_transactions: Vec<Vec<u8>>) {
        self.runtime_handle.spawn({
            let sender = self.sender.clone();
            async move {
                let res = sender.send(TransactionBatch::new(wire_transactions)).await;
                if res.is_err() {
                    warn!("Failed to send transaction to channel: it is closed.");
                }
            }
        });
    }

    #[cfg(any(test, feature = "dev-context-only-utils"))]
    fn protocol(&self) -> Protocol {
        Protocol::QUIC
    }
}

#[derive(Debug, Error)]
enum SendVoteError {
    #[error(transparent)]
    BincodeError(#[from] bincode::Error),
    #[error(transparent)]
    TransportError(#[from] TransportError),
}

// Helper for sending vote transaction with generic VoteClient
fn send_vote_transaction<V: VoteClient + ?Sized>(
    transaction: &Transaction,
    vote_client: &V,
) -> Result<(), SendVoteError> {
    let buf = serialize(transaction)?;
    vote_client.send_transactions_in_batch(vec![buf]);
    Ok(())
}

pub struct VotingService {
    thread_hdl: JoinHandle<()>,
}

#[derive(Clone)]
pub struct ClusterTpuInfo {
    cluster_info: Arc<ClusterInfo>,
    poh_recorder: Arc<RwLock<PohRecorder>>,
    recent_peers: HashMap<Pubkey, (SocketAddr, SocketAddr)>, // values are socket address for UDP and QUIC protocols
}

impl ClusterTpuInfo {
    pub fn new(cluster_info: Arc<ClusterInfo>, poh_recorder: Arc<RwLock<PohRecorder>>) -> Self {
        Self {
            cluster_info,
            poh_recorder,
            recent_peers: HashMap::new(),
        }
    }
}

impl TpuInfo for ClusterTpuInfo {
    fn refresh_recent_peers(&mut self) {
        self.recent_peers = self
            .cluster_info
            .tpu_peers()
            .into_iter()
            .chain(once(self.cluster_info.my_contact_info()))
            .filter_map(|node| {
                Some((
                    *node.pubkey(),
                    (node.tpu(Protocol::UDP)?, node.tpu(Protocol::QUIC)?),
                ))
            })
            .collect();
    }

    fn get_leader_tpus(&self, max_count: u64, protocol: Protocol) -> Vec<&SocketAddr> {
        let recorder = self.poh_recorder.read().unwrap();
        let leaders: Vec<_> = (0..max_count)
            .filter_map(|i| recorder.leader_after_n_slots(i * NUM_CONSECUTIVE_LEADER_SLOTS))
            .collect();
        drop(recorder);
        let mut unique_leaders = vec![];
        for leader in leaders.iter() {
            if let Some(addr) = self.recent_peers.get(leader).map(|addr| match protocol {
                Protocol::UDP => &addr.0,
                Protocol::QUIC => &addr.1,
            }) {
                if !unique_leaders.contains(&addr) {
                    unique_leaders.push(addr);
                }
            }
        }
        unique_leaders
    }

    fn get_not_unique_leader_tpus(&self, max_count: u64, protocol: Protocol) -> Vec<&SocketAddr> {
        let recorder = self.poh_recorder.read().unwrap();
        let leader_pubkeys: Vec<_> = (0..max_count)
            .filter_map(|i| recorder.leader_after_n_slots(i * NUM_CONSECUTIVE_LEADER_SLOTS))
            .collect();
        drop(recorder);
        leader_pubkeys
            .iter()
            .filter_map(|leader_pubkey| {
                self.recent_peers
                    .get(leader_pubkey)
                    .map(|addr| match protocol {
                        Protocol::UDP => &addr.0,
                        Protocol::QUIC => &addr.1,
                    })
            })
            .collect()
    }
}

impl VotingService {
    pub fn new(
        vote_receiver: Receiver<VoteOp>,
        cluster_info: Arc<ClusterInfo>,
        poh_recorder: Arc<RwLock<PohRecorder>>,
        tower_storage: Arc<dyn TowerStorage>,
        vote_client: VoteClientOption,
        alpenglow_socket: Option<UdpSocket>,
        bank_forks: Arc<RwLock<BankForks>>,
    ) -> Self {
        // Pattern match and construct the concrete vote client type
        match vote_client {
            VoteClientOption::ConnectionCache(connection_cache) => {
                let my_tpu_address = cluster_info.my_contact_info().tpu_vote(Protocol::QUIC)
                    .unwrap_or_else(|| {
                        panic!("Vote client requires a valid TPU address, but none found in cluster info");
                    });
                let leader_info = Some(ClusterTpuInfo::new(
                    cluster_info.clone(),
                    poh_recorder.clone(),
                ));

                let vote_client = ConnectionCacheClient::new(
                    connection_cache,
                    my_tpu_address,
                    leader_info,
                    1, // Forward to the next leader only
                );

                let thread_hdl = Builder::new()
                    .name("solVoteService".to_string())
                    .spawn({
                        let cluster_info = cluster_info.clone();
                        let poh_recorder = poh_recorder.clone();
                        let tower_storage = tower_storage.clone();
                        move || {
                            for vote_op in vote_receiver.iter() {
                                Self::handle_vote(
                                    &cluster_info,
                                    &poh_recorder,
                                    tower_storage.as_ref(),
                                    vote_op,
                                    &vote_client,
                                );
                            }
                        }
                    })
                    .unwrap();
                Self { thread_hdl }
            }
            VoteClientOption::TpuClientNext(
                identity_keypair,
                vote_client_socket,
                client_runtime,
                cancel,
            ) => {
                let my_tpu_address = cluster_info.my_contact_info().tpu_vote(Protocol::QUIC)
                    .unwrap_or_else(|| {
                        panic!("Vote client requires a valid TPU address, but none found in cluster info");
                    });
                let leader_info = Some(ClusterTpuInfo::new(
                    cluster_info.clone(),
                    poh_recorder.clone(),
                ));

                let vote_client = TpuClientNextClient::new(
                    client_runtime,
                    my_tpu_address,
                    leader_info,
                    Some(identity_keypair),
                    vote_client_socket,
                    cancel,
                );

                let thread_hdl = Builder::new()
                    .name("solVoteService".to_string())
                    .spawn({
                let mut mock_alpenglow = alpenglow_socket.map(|s| {
                    MockAlpenglowConsensus::new(
                        s,
                        cluster_info.clone(),
                        EpochSpecs::from(bank_forks.clone()),
                    )
                });
                {
                        let cluster_info = cluster_info.clone();
                        let poh_recorder = poh_recorder.clone();
                        let tower_storage = tower_storage.clone();
                        move || {
                                for vote_op in vote_receiver.iter() {
                        // Figure out if we are casting a vote for a new slot, and what slot it is for
                        let vote_slot = match vote_op {
                            VoteOp::PushVote {
                                tx: _,
                                ref tower_slots,
                                ..
                            } => tower_slots.iter().copied().last(),
                            _ => None,
                        };
                        // perform all the normal vote handling routines
                                    Self::handle_vote(
                                        &cluster_info,
                                        &poh_recorder,
                                        tower_storage.as_ref(),
                                        vote_op,
                                        &vote_client,
                                    );
                        // trigger mock alpenglow vote if we have just cast an actual vote
                        if let Some(slot) = vote_slot {
                            if let Some(ag) = mock_alpenglow.as_mut() {
                                let root_bank = { bank_forks.read().unwrap().root_bank() };
                                ag.signal_new_slot(slot, &root_bank);
                            }
                        }
                    }
                    if let Some(ag) = mock_alpenglow {
                        let _ = ag.join();
                    }
                            }
                        }
                    })
                    .unwrap();
                Self { thread_hdl }
            }
        }
    }

    // Generic version of handle_vote
    pub fn handle_vote<V: VoteClient + Send + Sync>(
        cluster_info: &ClusterInfo,
        poh_recorder: &RwLock<PohRecorder>,
        tower_storage: &dyn TowerStorage,
        vote_op: VoteOp,
        vote_client: &V,
    ) {
        if let VoteOp::PushVote { saved_tower, .. } = &vote_op {
            let mut measure = Measure::start("tower storage save");
            if let Err(err) = tower_storage.store(saved_tower) {
                error!("Unable to save tower to storage: {err:?}");
                std::process::exit(1);
            }
            measure.stop();
            trace!("{measure}");
        }

        // Attempt to send our vote transaction to the leaders for the next few
        // slots. From the current slot to the forwarding slot offset
        // (inclusive).
        const UPCOMING_LEADER_FANOUT_SLOTS: u64 =
            FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET.saturating_add(1);
        #[cfg(test)]
        static_assertions::const_assert_eq!(UPCOMING_LEADER_FANOUT_SLOTS, 3);
        let _upcoming_leader_sockets = upcoming_leader_tpu_vote_sockets(
            cluster_info,
            poh_recorder,
            UPCOMING_LEADER_FANOUT_SLOTS,
            vote_client.protocol(),
        );

        let _ = send_vote_transaction(vote_op.tx(), vote_client);

        match vote_op {
            VoteOp::PushVote {
                tx, tower_slots, ..
            } => {
                cluster_info.push_vote(&tower_slots, tx);
            }
            VoteOp::RefreshVote {
                tx,
                last_voted_slot,
            } => {
                cluster_info.refresh_vote(tx, last_voted_slot);
            }
        }
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread_hdl.join()
    }
}
