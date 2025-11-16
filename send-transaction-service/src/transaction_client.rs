use {
    crate::{send_transaction_service_stats::SendTransactionServiceStats, tpu_info::TpuInfo},
    async_trait::async_trait,
    log::warn,
    solana_keypair::Keypair,
    solana_measure::measure::Measure,
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
        net::{SocketAddr, UdpSocket},
        sync::atomic::Ordering,
        time::{Duration, Instant},
    },
    tokio::{
        runtime::Handle,
        sync::{
            mpsc::{self},
            watch,
        },
    },
    tokio_util::sync::CancellationToken,
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
#[derive(Clone)]
pub struct TpuClientNextClient {
    runtime_handle: Handle,
    sender: mpsc::Sender<TransactionBatch>,
    update_certificate_sender: watch::Sender<Option<StakeIdentity>>,
    #[cfg(any(test, feature = "dev-context-only-utils"))]
    cancel: CancellationToken,
}

const METRICS_REPORTING_INTERVAL: Duration = Duration::from_secs(3);
impl TpuClientNextClient {
    pub fn new<T>(
        runtime_handle: Handle,
        my_tpu_address: SocketAddr,
        tpu_peers: Option<Vec<SocketAddr>>,
        leader_info: Option<T>,
        leader_forward_count: u64,
        identity: Option<&Keypair>,
        bind_socket: UdpSocket,
        cancel: CancellationToken,
    ) -> Self
    where
        T: TpuInfoWithSendStatic + Clone,
    {
        // The channel size represents 8s worth of transactions at a rate of
        // 1000 tps, assuming batch size is 64.
        let (sender, receiver) = mpsc::channel(128);

        let (update_certificate_sender, update_certificate_receiver) = watch::channel(None);

        let leader_info_provider = CurrentLeaderInfo::new(leader_info);
        let leader_updater: SendTransactionServiceLeaderUpdater<T> =
            SendTransactionServiceLeaderUpdater {
                leader_info_provider,
                my_tpu_address,
                tpu_peers,
            };
        let config = Self::create_config(bind_socket, identity, leader_forward_count as usize);

        let scheduler = ConnectionWorkersScheduler::new(
            Box::new(leader_updater),
            receiver,
            update_certificate_receiver,
            cancel.clone(),
        );
        // leaking handle to this task, as it will run until the cancel signal is received
        runtime_handle.spawn(scheduler.get_stats().report_to_influxdb(
            "send-transaction-service-TPU-client",
            METRICS_REPORTING_INTERVAL,
            cancel.clone(),
        ));
        let _handle = runtime_handle.spawn(scheduler.run(config));
        Self {
            runtime_handle,
            sender,
            update_certificate_sender,
            #[cfg(any(test, feature = "dev-context-only-utils"))]
            cancel,
        }
    }

    fn create_config(
        bind_socket: UdpSocket,
        stake_identity: Option<&Keypair>,
        leader_forward_count: usize,
    ) -> ConnectionWorkersSchedulerConfig {
        ConnectionWorkersSchedulerConfig {
            bind: BindTarget::Socket(bind_socket),
            stake_identity: stake_identity.map(StakeIdentity::new),
            num_connections: MAX_CONNECTIONS,
            skip_check_transaction_age: true,
            // experimentally found parameter values
            worker_channel_size: 64,
            max_reconnect_attempts: 4,
            // We open connection to one more leader in advance, which time-wise means ~1.6s
            leaders_fanout: Fanout {
                connect: leader_forward_count + 1,
                send: leader_forward_count,
            },
        }
    }

    #[cfg(any(test, feature = "dev-context-only-utils"))]
    pub fn cancel(&self) {
        self.cancel.cancel();
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

impl TransactionClient for TpuClientNextClient {
    fn send_transactions_in_batch(
        &self,
        wire_transactions: Vec<Vec<u8>>,
        stats: &SendTransactionServiceStats,
    ) {
        let mut measure = Measure::start("send-us");
        self.runtime_handle.spawn({
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
            .map(|leader_info| leader_info.get_not_unique_leader_tpus(lookahead_leaders as u64))
            .filter(|addresses| !addresses.is_empty())
            .unwrap_or_else(|| vec![&self.my_tpu_address]);
        let mut all_peers = self.tpu_peers.clone().unwrap_or_default();
        all_peers.extend(discovered_peers.into_iter().cloned());
        all_peers
    }
    async fn stop(&mut self) {}
}
