use {
    crate::{send_transaction_service_stats::SendTransactionServiceStats, tpu_info::TpuInfo},
    async_trait::async_trait,
    log::warn,
    solana_keypair::Keypair,
    solana_measure::measure::Measure,
    solana_tls_utils::NotifyKeyUpdate,
    solana_tpu_client_next::{
        Client, ClientBuilder, ClientError, TransactionSender, leader_updater::LeaderUpdater,
    },
    std::{
        net::{SocketAddr, UdpSocket},
        num::NonZeroUsize,
        sync::atomic::Ordering,
        time::{Duration, Instant},
    },
    tokio::runtime::Handle,
    tokio_util::sync::CancellationToken,
};

/// How many connections to maintain the tpu-client-next cache. The value is
/// chosen to match MAX_CONNECTIONS from ConnectionCache
const MAX_CONNECTIONS: NonZeroUsize = NonZeroUsize::new(1024).unwrap();

// Alias trait to shorten function definitions.
pub trait TpuInfoWithSendStatic: TpuInfo + std::marker::Send + 'static {}
impl<T> TpuInfoWithSendStatic for T where T: TpuInfo + std::marker::Send + 'static {}

/// The leader info refresh rate.
pub const LEADER_INFO_REFRESH_RATE_MS: u64 = 1000;

const METRICS_REPORTING_INTERVAL: Duration = Duration::from_secs(3);

/// A synchronous adapter that schedules transaction batches on a Tokio runtime.
#[derive(Clone)]
pub struct TpuSender {
    runtime_handle: Handle,
    sender: TransactionSender,
}

impl TpuSender {
    pub fn send_transactions_in_batch(
        &self,
        wire_transactions: Vec<Vec<u8>>,
        stats: &SendTransactionServiceStats,
    ) {
        let mut measure = Measure::start("send-us");
        self.runtime_handle.spawn({
            let sender = self.sender.clone();
            async move {
                if sender
                    .send_transactions_in_batch(wire_transactions)
                    .await
                    .is_err()
                {
                    warn!("Failed to send transactions to channel: it is closed.");
                }
            }
        });

        measure.stop();
        stats.send_us.fetch_add(measure.as_us(), Ordering::Relaxed);
        stats.send_attempt_count.fetch_add(1, Ordering::Relaxed);
    }
}

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

pub struct TpuClient(Client);

impl TpuClient {
    pub async fn shutdown(self) -> Result<(), ClientError> {
        self.0.shutdown().await
    }
}

impl NotifyKeyUpdate for TpuClient {
    fn update_key(&self, identity: &Keypair) -> Result<(), Box<dyn core::error::Error>> {
        self.0
            .update_identity(identity)
            .map_err(|e| Box::new(e) as Box<dyn core::error::Error>)
    }
}

pub fn create_client(
    runtime_handle: Handle,
    leader_updater: Box<dyn LeaderUpdater>,
    leader_forward_count: u64,
    identity: Option<&Keypair>,
    bind_socket: UdpSocket,
    cancel: CancellationToken,
) -> Result<(TpuSender, TpuClient), String> {
    let sender_runtime_handle = runtime_handle.clone();
    let client_builder = ClientBuilder::new(leader_updater)
        .runtime_handle(runtime_handle)
        .bind_socket(bind_socket)
        .leader_send_fanout(leader_forward_count as usize)
        .identity(identity)
        .max_cache_size(MAX_CONNECTIONS)
        .cancel_token(cancel)
        .worker_channel_size(64)
        .sender_channel_size(128)
        .max_reconnect_attempts(4)
        .metric_reporter(|stats, cancel| async move {
            stats
                .report_to_influxdb(
                    "send-transaction-service-TPU-client",
                    METRICS_REPORTING_INTERVAL,
                    cancel,
                )
                .await;
        });

    let (sender, client) = client_builder.build().map_err(|e| e.to_string())?;
    Ok((
        TpuSender {
            runtime_handle: sender_runtime_handle,
            sender,
        },
        TpuClient(client),
    ))
}

struct SendTransactionServiceLeaderUpdater<T: TpuInfoWithSendStatic> {
    leader_info_provider: CurrentLeaderInfo<T>,
    my_tpu_address: SocketAddr,
    tpu_peers: Option<Vec<SocketAddr>>,
}

pub fn create_leader_updater<T: TpuInfoWithSendStatic>(
    leader_info: Option<T>,
    my_tpu_address: SocketAddr,
    tpu_peers: Option<Vec<SocketAddr>>,
) -> Box<dyn LeaderUpdater> {
    Box::new(SendTransactionServiceLeaderUpdater {
        leader_info_provider: CurrentLeaderInfo::new(leader_info),
        my_tpu_address,
        tpu_peers,
    })
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
            .map(|leader_info| leader_info.get_not_unique_leader_tpus(lookahead_leaders as u64))
            .filter(|addresses| !addresses.is_empty())
            .unwrap_or_else(|| vec![&self.my_tpu_address]);
        let mut all_peers = self.tpu_peers.clone().unwrap_or_default();
        all_peers.extend(discovered_peers.into_iter().cloned());
        all_peers
    }
    async fn stop(&mut self) {}
}
