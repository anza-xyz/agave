use {
    crate::next_leader::upcoming_leader_tpu_vote_sockets,
    async_trait::async_trait,
    solana_client::connection_cache::ConnectionCache,
    solana_clock::NUM_CONSECUTIVE_LEADER_SLOTS,
    solana_connection_cache::client_connection::ClientConnection,
    solana_gossip::{cluster_info::ClusterInfo, contact_info::Protocol},
    solana_keypair::Keypair,
    solana_poh::poh_recorder::PohRecorder,
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
    std::{
        collections::HashMap,
        iter::once,
        net::{SocketAddr, UdpSocket},
        sync::{Arc, Mutex, RwLock},
        time::{Duration, Instant},
    },
    tokio::sync::{mpsc, watch},
    tokio_util::sync::CancellationToken,
};

#[derive(Clone)]
pub struct VoteClientLeaderUpdater<T: TpuVoteInfoWithSendStatic> {
    leader_info_provider: CurrentLeaderInfo<T>,
    my_tpu_vote_address: SocketAddr,
}

#[async_trait]
impl<T> LeaderUpdater for VoteClientLeaderUpdater<T>
where
    T: TpuVoteInfoWithSendStatic,
{
    fn next_leaders(&mut self, lookahead_leaders: usize) -> Vec<SocketAddr> {
        self.leader_info_provider
            .get_leader_info()
            .map(|leader_info| {
                leader_info
                    .get_not_unique_leader_tpu_votes(lookahead_leaders as u64, Protocol::QUIC)
                    .into_iter()
                    .cloned()
                    .collect::<Vec<SocketAddr>>()
            })
            .filter(|addresses| !addresses.is_empty())
            .unwrap_or_else(|| vec![self.my_tpu_vote_address])
    }
    async fn stop(&mut self) {}
}

/// A trait to abstract out the leader estimation for the vote
pub trait TpuVoteInfo {
    fn refresh_recent_peers(&mut self);

    /// Takes `fanout_slots` which specifies how many leaders
    /// TPU vote socket addresses for these leaders.
    ///
    /// For example, if leader schedule was `[L1, L1, L1, L1, L2, L2, L2, L2,
    /// L1, ...]` it will return `[L1, L2]` (the last L1 will be not added to
    /// the result).
    fn get_leader_tpu_votes(&self, fanout_slots: u64, protocol: Protocol) -> Vec<SocketAddr>;

    /// Takes `max_count` which specifies how many leaders per
    /// `NUM_CONSECUTIVE_LEADER_SLOTS` we want to receive and returns TPU vote socket
    /// addresses for these leaders.
    ///
    /// For example, if leader schedule was `[L1, L1, L1, L1, L2, L2, L2, L2,
    /// L1, ...]` it will return `[L1, L2, L1]`.
    fn get_not_unique_leader_tpu_votes(
        &self,
        max_count: u64,
        protocol: Protocol,
    ) -> Vec<&SocketAddr>;
}

// Alias trait to shorten function definitions.
pub trait TpuVoteInfoWithSendStatic: TpuVoteInfo + std::marker::Send + 'static {}
impl<T> TpuVoteInfoWithSendStatic for T where T: TpuVoteInfo + std::marker::Send + 'static {}

pub trait VoteClient {
    fn send_transactions_in_batch(&self, wire_transactions: Vec<Vec<u8>>);

    fn protocol(&self) -> Protocol;
}

/// The leader info refresh rate.
pub const LEADER_INFO_REFRESH_RATE_MS: u64 = 1000;

/// A struct responsible for holding up-to-date leader information
/// used for sending transactions.
#[derive(Clone)]
pub struct CurrentLeaderInfo<T>
where
    T: TpuVoteInfoWithSendStatic,
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
    T: TpuVoteInfoWithSendStatic,
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

pub struct ConnectionCacheClient<T: TpuVoteInfoWithSendStatic> {
    connection_cache: Arc<ConnectionCache>,
    tpu_vote_address: SocketAddr,
    leader_info_provider: Arc<Mutex<CurrentLeaderInfo<T>>>,
    leader_fanout_slots: u64,
}

// Manual implementation of Clone without requiring T to be Clone
impl<T> Clone for ConnectionCacheClient<T>
where
    T: TpuVoteInfoWithSendStatic,
{
    fn clone(&self) -> Self {
        Self {
            connection_cache: Arc::clone(&self.connection_cache),
            tpu_vote_address: self.tpu_vote_address,
            leader_info_provider: Arc::clone(&self.leader_info_provider),
            leader_fanout_slots: self.leader_fanout_slots,
        }
    }
}

impl<T> ConnectionCacheClient<T>
where
    T: TpuVoteInfoWithSendStatic,
{
    pub fn new(
        connection_cache: Arc<ConnectionCache>,
        tpu_address: SocketAddr,
        leader_info: Option<T>,
        leader_fanout_slots: u64,
    ) -> Self {
        let leader_info_provider = Arc::new(Mutex::new(CurrentLeaderInfo::new(leader_info)));
        Self {
            connection_cache,
            tpu_vote_address: tpu_address,
            leader_info_provider,
            leader_fanout_slots,
        }
    }

    fn get_tpu_addresses<'a>(&'a self, leader_info: Option<&'a T>) -> Vec<SocketAddr> {
        leader_info
            .map(|leader_info| {
                leader_info.get_leader_tpu_votes(
                    self.leader_fanout_slots,
                    self.connection_cache.protocol(),
                )
            })
            .filter(|addresses| !addresses.is_empty())
            .unwrap_or_else(|| vec![self.tpu_vote_address])
    }

    fn send_transactions(&self, peer: &SocketAddr, wire_transactions: Vec<Vec<u8>>) {
        let conn = self.connection_cache.get_connection(peer);
        let result = conn.send_data_batch_async(wire_transactions);

        if let Err(err) = result {
            warn!(
                "Failed to send transaction transaction to {}: {:?}",
                self.tpu_vote_address, err
            );
        }
    }
}

impl<T> VoteClient for ConnectionCacheClient<T>
where
    T: TpuVoteInfoWithSendStatic,
{
    fn send_transactions_in_batch(&self, wire_transactions: Vec<Vec<u8>>) {
        let mut leader_info_provider = self.leader_info_provider.lock().unwrap();
        let leader_info = leader_info_provider.get_leader_info();
        let leader_addresses = self.get_tpu_addresses(leader_info);

        for address in &leader_addresses {
            self.send_transactions(address, wire_transactions.clone());
        }
    }

    fn protocol(&self) -> Protocol {
        self.connection_cache.protocol()
    }
}

#[derive(Clone)]
pub struct TpuClientNextVoteClient {
    runtime_handle: tokio::runtime::Handle,
    sender: mpsc::Sender<TransactionBatch>,
    update_certificate_sender: watch::Sender<Option<StakeIdentity>>,
}

const METRICS_REPORTING_INTERVAL: Duration = Duration::from_secs(5);

impl TpuClientNextVoteClient {
    pub fn new<T>(
        runtime_handle: tokio::runtime::Handle,
        my_tpu_address: SocketAddr,
        leader_info: Option<T>,
        stake_identity: Option<&Keypair>,
        bind_socket: UdpSocket,
        leader_fanout_slots: u64,
        cancel: CancellationToken,
    ) -> Self
    where
        T: TpuVoteInfoWithSendStatic + Clone,
    {
        // For now use large channel, the more suitable size to be found later.
        let (sender, receiver) = mpsc::channel(128);
        let leader_info_provider = CurrentLeaderInfo::new(leader_info);

        let leader_updater: VoteClientLeaderUpdater<T> = VoteClientLeaderUpdater {
            leader_info_provider,
            my_tpu_vote_address: my_tpu_address,
        };

        let config = Self::create_config(bind_socket, stake_identity, leader_fanout_slots);
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
        leader_fanout_slots: u64,
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
            leaders_fanout: Fanout {
                send: leader_fanout_slots as usize,
                connect: 4,
            },
        }
    }
}

impl NotifyKeyUpdate for TpuClientNextVoteClient {
    fn update_key(&self, identity: &Keypair) -> Result<(), Box<dyn std::error::Error>> {
        let stake_identity = StakeIdentity::new(identity);
        self.update_certificate_sender
            .send(Some(stake_identity))
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
    }
}

impl VoteClient for TpuClientNextVoteClient {
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

    fn protocol(&self) -> Protocol {
        Protocol::QUIC
    }
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

impl TpuVoteInfo for ClusterTpuInfo {
    fn refresh_recent_peers(&mut self) {
        self.recent_peers = self
            .cluster_info
            .tpu_peers()
            .into_iter()
            .chain(once(self.cluster_info.my_contact_info()))
            .filter_map(|node| {
                Some((
                    *node.pubkey(),
                    (
                        node.tpu_vote(Protocol::UDP)?,
                        node.tpu_vote(Protocol::QUIC)?,
                    ),
                ))
            })
            .collect();
    }

    fn get_leader_tpu_votes(&self, fanout_slots: u64, protocol: Protocol) -> Vec<SocketAddr> {
        upcoming_leader_tpu_vote_sockets(
            &self.cluster_info,
            &self.poh_recorder,
            fanout_slots,
            protocol,
        )
    }

    fn get_not_unique_leader_tpu_votes(
        &self,
        max_count: u64,
        protocol: Protocol,
    ) -> Vec<&SocketAddr> {
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
