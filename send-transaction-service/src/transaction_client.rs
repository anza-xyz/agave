use {
    crate::{send_transaction_service_stats::SendTransactionServiceStats, tpu_info::TpuInfo},
    async_trait::async_trait,
    log::warn,
    solana_client::connection_cache::{ConnectionCache, Protocol},
    solana_connection_cache::client_connection::ClientConnection as TpuConnection,
    solana_measure::measure::Measure,
    solana_sdk::signature::Keypair,
    solana_tpu_client_next::{
        connection_workers_scheduler::{ConnectionWorkersSchedulerConfig, Fanout},
        leader_updater::LeaderUpdater,
        transaction_batch::TransactionBatch,
        ConnectionWorkersScheduler,
    },
    std::{
        net::{Ipv4Addr, SocketAddr},
        sync::{atomic::Ordering, Arc, Mutex},
        time::{Duration, Instant},
    },
    tokio::{
        runtime::Handle,
        sync::mpsc::{self, Sender},
    },
    tokio_util::sync::CancellationToken,
};

// Alias trait to shorten function definitions.
pub trait TpuInfoWithSendStatic: TpuInfo + std::marker::Send + 'static {}
impl<T> TpuInfoWithSendStatic for T where T: TpuInfo + std::marker::Send + 'static {}

pub trait TransactionClient {
    fn send_transactions_in_batch(
        &self,
        wire_transactions: Vec<Vec<u8>>,
        stats: &SendTransactionServiceStats,
    );

    fn protocol(&self) -> Protocol;
}

pub trait Cancelable {
    fn cancel(&self);
}

pub struct ConnectionCacheClient<T: TpuInfoWithSendStatic> {
    connection_cache: Arc<ConnectionCache>,
    tpu_address: SocketAddr,
    tpu_peers: Option<Vec<SocketAddr>>,
    leader_info_provider: Arc<Mutex<CurrentLeaderInfo<T>>>,
    leader_forward_count: u64,
}

// Manual implementation of Clone to avoid requiring T to be Clone
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

    fn get_tpu_addresses_with_slots<'a>(
        &'a self,
        leader_info: Option<&'a T>,
    ) -> Vec<&'a SocketAddr> {
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
        let leader_addresses = self.get_tpu_addresses_with_slots(leader_info);
        addresses.extend(leader_addresses);

        for address in &addresses {
            self.send_transactions(address, wire_transactions.clone(), stats);
        }
    }

    fn protocol(&self) -> Protocol {
        self.connection_cache.protocol()
    }
}

impl<T> Cancelable for ConnectionCacheClient<T>
where
    T: TpuInfoWithSendStatic,
{
    fn cancel(&self) {}
}

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
    fn next_leaders(&mut self, lookahead_slots: usize) -> Vec<SocketAddr> {
        // it is &mut because it is not a service! so it needs to update the state
        // so i had to change the interface
        let discovered_peers = self
            .leader_info_provider
            .get_leader_info()
            .map(|leader_info| leader_info.get_leader_tpus(lookahead_slots as u64, Protocol::QUIC))
            .filter(|addresses| !addresses.is_empty())
            .unwrap_or_else(|| vec![&self.my_tpu_address]);
        let mut all_peers = self.tpu_peers.clone().unwrap_or_default();
        all_peers.extend(discovered_peers.into_iter().cloned());
        all_peers
    }
    async fn stop(&mut self) {}
}

#[derive(Clone)]
pub struct TpuClientNextClient {
    sender: Sender<TransactionBatch>,
    cancel: CancellationToken,
}

impl Cancelable for TpuClientNextClient {
    fn cancel(&self) {
        self.cancel.cancel();
    }
}

impl TransactionClient for TpuClientNextClient {
    fn send_transactions_in_batch(
        &self,
        wire_transactions: Vec<Vec<u8>>,
        stats: &SendTransactionServiceStats,
    ) {
        let res = self
            .sender
            .try_send(TransactionBatch::new(wire_transactions));
        match res {
            Ok(_) => {
                stats.send_attempt_count.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                warn!("Failed to send transaction transaction, transaction chanel is full.");
                stats.send_failure_count.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn protocol(&self) -> Protocol {
        Protocol::QUIC
    }
}

pub fn spawn_tpu_client_send_txs<T>(
    runtime_handle: Handle,
    my_tpu_address: SocketAddr,
    tpu_peers: Option<Vec<SocketAddr>>,
    leader_info: Option<T>,
    leader_forward_count: u64,
    validator_identity: Option<Keypair>,
) -> TpuClientNextClient
where
    T: TpuInfoWithSendStatic,
{
    let leader_info_provider = CurrentLeaderInfo::new(leader_info);

    let (sender, receiver) = mpsc::channel(16);
    let cancel = CancellationToken::new();
    let _handle = runtime_handle.spawn({
        let cancel = cancel.clone();
        async move {
            let leader_updater: SendTransactionServiceLeaderUpdater<T> =
                SendTransactionServiceLeaderUpdater {
                    leader_info_provider,
                    my_tpu_address,
                    tpu_peers,
                };
            let config = ConnectionWorkersSchedulerConfig {
                bind: SocketAddr::new(Ipv4Addr::new(0, 0, 0, 0).into(), 0),
                stake_identity: validator_identity,
                // to match MAX_CONNECTIONS from ConnectionCache
                num_connections: 1024,
                skip_check_transaction_age: true,
                worker_channel_size: 64,
                max_reconnect_attempts: 4,
                leaders_fanout: Fanout {
                    connect: leader_forward_count as usize,
                    send: leader_forward_count as usize,
                },
            };
            let _scheduler = tokio::spawn(ConnectionWorkersScheduler::run(
                config,
                Box::new(leader_updater),
                receiver,
                cancel.clone(),
            ));
        }
    });
    TpuClientNextClient { sender, cancel }
}

/// The leader info refresh rate.
pub const LEADER_INFO_REFRESH_RATE_MS: u64 = 1000;

/// A struct responsible for holding up-to-date leader information
/// used for sending transactions.
pub(crate) struct CurrentLeaderInfo<T>
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
