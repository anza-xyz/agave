use {
    crate::{send_transaction_service_stats::SendTransactionServiceStats, tpu_info::TpuInfo},
    async_trait::async_trait,
    log::warn,
    solana_client::connection_cache::{ConnectionCache, Protocol},
    solana_connection_cache::client_connection::ClientConnection as TpuConnection,
    solana_measure::measure::Measure,
    solana_tpu_client_next::{
        connection_workers_scheduler::ConnectionWorkersSchedulerConfig,
        leader_updater::LeaderUpdater, transaction_batch::TransactionBatch,
        ConnectionWorkersScheduler,
    },
    std::{
        net::{Ipv4Addr, SocketAddr},
        sync::{atomic::Ordering, Arc, Mutex},
        time::{Duration, Instant},
    },
    tokio::{
        runtime::Runtime,
        sync::mpsc::{self, Sender},
    },
    tokio_util::sync::CancellationToken,
};

pub trait TransactionClient {
    fn send_transactions_in_batch(
        &self,
        wire_transactions: Vec<Vec<u8>>,
        stats: &SendTransactionServiceStats,
    );
}

pub struct ConnectionCacheClient<T: TpuInfo + std::marker::Send + 'static> {
    connection_cache: Arc<ConnectionCache>,
    tpu_address: SocketAddr,
    tpu_peers: Option<Vec<SocketAddr>>,
    leader_info_provider: Arc<Mutex<CurrentLeaderInfo<T>>>,
    leader_forward_count: u64,
}

// Manual implementation of Clone without requiring T to be Clone
impl<T> Clone for ConnectionCacheClient<T>
where
    T: TpuInfo + std::marker::Send + 'static,
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
    T: TpuInfo + std::marker::Send + 'static,
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
    T: TpuInfo + std::marker::Send + 'static,
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
}

pub struct SendTransactionServiceLeaderUpdater<T: TpuInfo + std::marker::Send + 'static> {
    leader_info_provider: CurrentLeaderInfo<T>,
    my_tpu_address: SocketAddr,
    tpu_peers: Option<Vec<SocketAddr>>,
}

//TODO(klykov): it is should not be async trait because we don't need here it to be stoppable
#[async_trait]
impl<T> LeaderUpdater for SendTransactionServiceLeaderUpdater<T>
where
    T: TpuInfo + std::marker::Send + 'static,
{
    //TODO(klykov): So each call will lead to shit tons of allocations, I think
    //we should recalculate only once a slot or something?
    fn next_leaders(&mut self, lookahead_slots: u64) -> Vec<SocketAddr> {
        // it is &mut because it is not a service! so it needs to update the state
        // so i had to change the interface
        let discovered_peers = self
            .leader_info_provider
            .get_leader_info()
            .map(|leader_info| leader_info.get_leader_tpus(lookahead_slots / 4, Protocol::QUIC))
            .filter(|addresses| !addresses.is_empty())
            .unwrap_or_else(|| vec![&self.my_tpu_address]);
        let mut all_peers = self.tpu_peers.clone().unwrap_or_default();
        all_peers.extend(discovered_peers.into_iter().cloned());
        all_peers
    }
    async fn stop(&mut self) {}
}

struct TpuClientNextClient {
    sender: Sender<TransactionBatch>,
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
}

pub(crate) fn spawn_tpu_client_send_txs<T>(
    runtime: Runtime,
    my_tpu_address: SocketAddr,
    tpu_peers: Option<Vec<SocketAddr>>,
    leader_info: Option<T>,
    leader_forward_count: u64,
) -> Sender<TransactionBatch>
where
    T: TpuInfo + std::marker::Send + 'static,
{
    let leader_info_provider = CurrentLeaderInfo::new(leader_info);

    let (transaction_sender, transaction_receiver) = mpsc::channel(16); // random number of now
    let validator_identity = None;
    runtime.spawn(async move {
        let cancel = CancellationToken::new();
        let leader_updater = SendTransactionServiceLeaderUpdater {
            leader_info_provider,
            my_tpu_address,
            tpu_peers,
        };
        let config = ConnectionWorkersSchedulerConfig {
            bind: SocketAddr::new(Ipv4Addr::new(0, 0, 0, 0).into(), 0),
            stake_identity: validator_identity, //TODO In CC, do we send with identity?
            num_connections: 1,
            skip_check_transaction_age: true,
            worker_channel_size: 2,
            max_reconnect_attempts: 4,
            lookahead_slots: leader_forward_count,
        };
        let _scheduler = tokio::spawn(ConnectionWorkersScheduler::run(
            config,
            Box::new(leader_updater),
            transaction_receiver,
            cancel.clone(),
        ));
    });
    transaction_sender
}

/// The leader info refresh rate.
pub const LEADER_INFO_REFRESH_RATE_MS: u64 = 1000;

/// A struct responsible for holding up-to-date leader information
/// used for sending transactions.
pub(crate) struct CurrentLeaderInfo<T>
where
    T: TpuInfo + std::marker::Send + 'static,
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
    T: TpuInfo + std::marker::Send + 'static,
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
