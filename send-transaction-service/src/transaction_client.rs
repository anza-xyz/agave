use {
    crate::{send_transaction_service_stats::SendTransactionServiceStats, tpu_info::TpuInfo},
    async_trait::async_trait,
    log::warn,
    solana_client::connection_cache::{ConnectionCache, Protocol},
    solana_connection_cache::client_connection::ClientConnection as TpuConnection,
    solana_keypair::Keypair,
    solana_measure::measure::Measure,
    solana_sdk::quic::NotifyKeyUpdate,
    solana_tpu_client_next::{
        client_key_updater::ClientKeyUpdater,
        connection_workers_scheduler::{BindTarget, ConnectionWorkersSchedulerConfig, Fanout},
        leader_updater::LeaderUpdater,
        transaction_batch::TransactionBatch,
    },
    std::{
        net::{SocketAddr, UdpSocket},
        sync::{atomic::Ordering, Arc, Mutex},
        time::{Duration, Instant},
    },
    tokio::{
        runtime::Handle,
        sync::mpsc::{self},
    },
};

/// How many connections to maintain the tpu-client-next cache. The value is
/// chosen to match MAX_CONNECTIONS from ConnectionCache
const MAX_CONNECTIONS: usize = 1024;

// Alias trait to shorten function definitions.
pub trait TpuInfoWithSendStatic: TpuInfo + std::marker::Send + 'static {}
impl<T> TpuInfoWithSendStatic for T where T: TpuInfo + std::marker::Send + 'static {}

pub trait TransactionClient {
    fn send_transactions_in_batch(
        &self,
        wire_transactions: Vec<Vec<u8>>,
        stats: &SendTransactionServiceStats,
    );

    #[cfg(any(test, feature = "dev-context-only-utils"))]
    fn protocol(&self) -> Protocol;

    fn exit(&self);
}

pub struct ConnectionCacheClient<T: TpuInfoWithSendStatic> {
    connection_cache: Arc<ConnectionCache>,
    tpu_address: SocketAddr,
    tpu_peers: Option<Vec<SocketAddr>>,
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
            tpu_peers: self.tpu_peers.clone(),
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
        tpu_peers: Option<Vec<SocketAddr>>,
        leader_info: Option<T>,
        leader_forward_count: u64,
    ) -> Self {
        let leader_info_provider = Arc::new(Mutex::new(CurrentLeaderInfo::new(leader_info)));
        Self {
            connection_cache,
            tpu_address,
            tpu_peers,
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

    fn send_transactions(
        &self,
        peer: &SocketAddr,
        wire_transactions: Vec<Vec<u8>>,
        stats: &SendTransactionServiceStats,
    ) {
        let mut measure = Measure::start("send-us");
        let conn = self.connection_cache.get_connection(peer);
        let result = conn.send_data_batch_async(wire_transactions);

        if let Err(err) = result {
            warn!(
                "Failed to send transaction transaction to {}: {:?}",
                self.tpu_address, err
            );
            stats.send_failure_count.fetch_add(1, Ordering::Relaxed);
        }

        measure.stop();
        stats.send_us.fetch_add(measure.as_us(), Ordering::Relaxed);
        stats.send_attempt_count.fetch_add(1, Ordering::Relaxed);
    }
}

impl<T> TransactionClient for ConnectionCacheClient<T>
where
    T: TpuInfoWithSendStatic,
{
    fn send_transactions_in_batch(
        &self,
        wire_transactions: Vec<Vec<u8>>,
        stats: &SendTransactionServiceStats,
    ) {
        // Processing the transactions in batch
        let mut addresses = self
            .tpu_peers
            .as_ref()
            .map(|addrs| addrs.iter().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut leader_info_provider = self.leader_info_provider.lock().unwrap();
        let leader_info = leader_info_provider.get_leader_info();
        let leader_addresses = self.get_tpu_addresses(leader_info);
        addresses.extend(leader_addresses);

        for address in &addresses {
            self.send_transactions(address, wire_transactions.clone(), stats);
        }
    }

    #[cfg(any(test, feature = "dev-context-only-utils"))]
    fn protocol(&self) -> Protocol {
        self.connection_cache.protocol()
    }

    fn exit(&self) {}
}

impl<T> NotifyKeyUpdate for ConnectionCacheClient<T>
where
    T: TpuInfoWithSendStatic,
{
    fn update_key(&self, identity: &Keypair) -> Result<(), Box<dyn std::error::Error>> {
        self.connection_cache.update_key(identity)
    }
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

/// `TpuClientNextClient` provides an interface for managing the
/// [`ConnectionWorkersScheduler`].
///
/// It allows:
/// * Create and initializes the scheduler with runtime configurations,
/// * Send transactions to the connection scheduler,
/// * Update the validator identity keypair and propagate the changes to the
///   scheduler. Most of the complexity of this structure arises from this
///   functionality.
pub struct TpuClientNextClient {
    key_update_helper: ClientKeyUpdater,
    bind_socket: UdpSocket,
    leader_forward_count: u64,
    sender: mpsc::Sender<TransactionBatch>,
}

// Implement Clone manually because `UdpSocket` implements only `try_clone`.
impl Clone for TpuClientNextClient {
    fn clone(&self) -> Self {
        let bind_socket = self
            .bind_socket
            .try_clone()
            .expect("Cloning bind socket should always finish successfully.");

        TpuClientNextClient {
            key_update_helper: self.key_update_helper.clone(),
            bind_socket,
            leader_forward_count: self.leader_forward_count,
            sender: self.sender.clone(),
        }
    }
}

impl TpuClientNextClient {
    pub fn new<T>(
        runtime_handle: Handle,
        my_tpu_address: SocketAddr,
        tpu_peers: Option<Vec<SocketAddr>>,
        leader_info: Option<T>,
        leader_forward_count: u64,
        identity: Option<&Keypair>,
        bind_socket: UdpSocket,
    ) -> Self
    where
        T: TpuInfoWithSendStatic + Clone,
    {
        // The channel size represents 8s worth of transactions at a rate of
        // 1000 tps, assuming batch size is 64.
        let (sender, receiver) = mpsc::channel(128);

        let leader_info_provider = CurrentLeaderInfo::new(leader_info);
        let leader_updater: SendTransactionServiceLeaderUpdater<T> =
            SendTransactionServiceLeaderUpdater {
                leader_info_provider,
                my_tpu_address,
                tpu_peers,
            };
        let config = Self::create_config(&bind_socket, identity, leader_forward_count as usize);
        let key_update_helper = ClientKeyUpdater::new(
            runtime_handle,
            receiver,
            leader_updater,
            config,
            "send-transaction-service-TPU-client",
        );
        Self {
            key_update_helper,
            leader_forward_count,
            bind_socket,
            sender,
        }
    }

    fn create_config(
        bind_socket: &UdpSocket,
        stake_identity: Option<&Keypair>,
        leader_forward_count: usize,
    ) -> ConnectionWorkersSchedulerConfig {
        let bind_socket = bind_socket
            .try_clone()
            .expect("Cloning bind socket should always finish successfully.");
        ConnectionWorkersSchedulerConfig {
            bind: BindTarget::Socket(bind_socket),
            stake_identity: stake_identity.map(Into::into),
            num_connections: MAX_CONNECTIONS,
            skip_check_transaction_age: true,
            // experimentally found parameter values
            worker_channel_size: 64,
            max_reconnect_attempts: 4,
            leaders_fanout: Fanout {
                connect: leader_forward_count,
                send: leader_forward_count,
            },
        }
    }

    #[cfg(any(test, feature = "dev-context-only-utils"))]
    pub fn cancel(&self) -> Result<(), Box<dyn std::error::Error>> {
        let Ok(lock) = self.key_update_helper.join_and_cancel.lock() else {
            return Err("Failed to stop scheduler.".into());
        };
        lock.1.cancel();
        Ok(())
    }
}

impl NotifyKeyUpdate for TpuClientNextClient {
    fn update_key(&self, identity: &Keypair) -> Result<(), Box<dyn std::error::Error>> {
        let config = Self::create_config(
            &self.bind_socket,
            Some(identity),
            self.leader_forward_count as usize,
        );
        self.key_update_helper.runtime_handle.block_on(
            self.key_update_helper
                .do_update_key(config, "send-transaction-service-TPU-client"),
        )
    }
}

impl TransactionClient for TpuClientNextClient {
    fn send_transactions_in_batch(
        &self,
        wire_transactions: Vec<Vec<u8>>,
        stats: &SendTransactionServiceStats,
    ) {
        let mut measure = Measure::start("send-us");
        self.key_update_helper.runtime_handle.spawn({
            let sender = self.sender.clone();
            async move {
                let res = sender.send(TransactionBatch::new(wire_transactions)).await;
                if res.is_err() {
                    warn!("Failed to send transaction to channel: it is closed.");
                }
            }
        });

        measure.stop();
        stats.send_us.fetch_add(measure.as_us(), Ordering::Relaxed);
        stats.send_attempt_count.fetch_add(1, Ordering::Relaxed);
    }

    #[cfg(any(test, feature = "dev-context-only-utils"))]
    fn protocol(&self) -> Protocol {
        Protocol::QUIC
    }

    fn exit(&self) {
        self.key_update_helper.exit();
    }
}

#[derive(Clone)]
pub struct SendTransactionServiceLeaderUpdater<T: TpuInfoWithSendStatic> {
    leader_info_provider: CurrentLeaderInfo<T>,
    my_tpu_address: SocketAddr,
    tpu_peers: Option<Vec<SocketAddr>>,
}

#[async_trait]
impl<T> LeaderUpdater for SendTransactionServiceLeaderUpdater<T>
where
    T: TpuInfoWithSendStatic,
{
    fn next_leaders(&mut self, lookahead_leaders: usize) -> Vec<SocketAddr> {
        let discovered_peers = self
            .leader_info_provider
            .get_leader_info()
            .map(|leader_info| {
                leader_info.get_not_unique_leader_tpus(lookahead_leaders as u64, Protocol::QUIC)
            })
            .filter(|addresses| !addresses.is_empty())
            .unwrap_or_else(|| vec![&self.my_tpu_address]);
        let mut all_peers = self.tpu_peers.clone().unwrap_or_default();
        all_peers.extend(discovered_peers.into_iter().cloned());
        all_peers
    }
    async fn stop(&mut self) {}
}
