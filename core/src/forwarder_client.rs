//! Client code that allows choosing the client for forwarding by wrapping
//! tpu-client-next and ConnectionCache.
// TODO Should it be inside banking stage?
use {
    crate::next_leader::next_leader,
    async_trait::async_trait,
    log::warn,
    solana_client::connection_cache::{ConnectionCache, Protocol},
    solana_connection_cache::client_connection::ClientConnection as TpuConnection,
    solana_gossip::cluster_info::ClusterInfo,
    solana_poh::poh_recorder::PohRecorder,
    solana_sdk::{quic::NotifyKeyUpdate, signature::Keypair},
    solana_tpu_client_next::{
        connection_workers_scheduler::{
            ConnectionWorkersSchedulerConfig, Fanout, TransactionStatsAndReceiver,
        },
        leader_updater::LeaderUpdater,
        transaction_batch::TransactionBatch,
        ConnectionWorkersScheduler, ConnectionWorkersSchedulerError, QuicClientCertificate,
    },
    std::{
        net::{Ipv4Addr, SocketAddr},
        sync::{Arc, Mutex, RwLock},
    },
    tokio::{
        runtime::Handle,
        sync::mpsc::{self},
        task::JoinHandle,
    },
    tokio_util::sync::CancellationToken,
};

pub trait ForwarderClient {
    fn send_transactions_in_batch(&self, wire_transactions: Vec<Vec<u8>>);

    fn protocol(&self) -> Protocol;
}

struct ForwarderConnectionCacheClient {
    connection_cache: Arc<ConnectionCache>,
    cluster_info: Arc<ClusterInfo>,
    poh_recorder: Arc<RwLock<PohRecorder>>,
}

impl ForwarderConnectionCacheClient {
    pub fn new(
        connection_cache: Arc<ConnectionCache>,
        cluster_info: Arc<ClusterInfo>,
        poh_recorder: Arc<RwLock<PohRecorder>>,
    ) -> Self {
        Self {
            connection_cache,
            cluster_info,
            poh_recorder,
        }
    }
}

impl ForwarderClient for ForwarderConnectionCacheClient {
    fn send_transactions_in_batch(&self, wire_transactions: Vec<Vec<u8>>) {
        let Some((pubkey, address)) = next_leader(&self.cluster_info, &self.poh_recorder, |node| {
            node.tpu_forwards(self.connection_cache.protocol())
        }) else {
            return;
        };
        let conn = self.connection_cache.get_connection(&address);
        let result = conn.send_data_batch_async(wire_transactions);

        if let Err(err) = result {
            warn!(
                "Failed to send transaction transaction to {}: {:?}",
                address, err
            );
        }
    }

    // Needed?
    fn protocol(&self) -> Protocol {
        self.connection_cache.protocol()
    }
}

impl NotifyKeyUpdate for ForwarderConnectionCacheClient {
    fn update_key(&self, validator_identity: &Keypair) -> Result<(), Box<dyn std::error::Error>> {
        self.connection_cache.update_key(validator_identity)
    }
}

#[derive(Clone)]
struct ForwarderLeaderUpdater {
    cluster_info: Arc<ClusterInfo>,
    poh_recorder: Arc<RwLock<PohRecorder>>,
}

#[async_trait]
impl LeaderUpdater for ForwarderLeaderUpdater {
    fn next_leaders(&mut self, _lookahead_leaders: usize) -> Vec<SocketAddr> {
        let Some((pubkey, address)) = next_leader(&self.cluster_info, &self.poh_recorder, |node| {
            node.tpu_forwards(Protocol::QUIC)
        }) else {
            return vec![];
        };
        vec![address]
    }
    async fn stop(&mut self) {}
}

type TpuClientJoinHandle =
    JoinHandle<Result<TransactionStatsAndReceiver, ConnectionWorkersSchedulerError>>;

/// This structure allows to control [`ConnectionWorkersScheduler`]:
/// * send transactions to scheduler
/// * create scheduler
/// * update validator identity keypair and update scheduler (implements [`NotifyKeyUpdate`]).
///  so that the underlying task sending
#[derive(Clone)]
pub struct ForwarderTpuClientNextClient {
    runtime_handle: Handle,
    sender: mpsc::Sender<TransactionBatch>,
    // This handle is needed to implement `NotifyKeyUpdate` trait. It's only
    // method takes &self and thus we need to wrap with Mutex
    join_and_cancel: Arc<Mutex<(Option<TpuClientJoinHandle>, CancellationToken)>>,
    leader_updater: ForwarderLeaderUpdater,
}

impl ForwarderTpuClientNextClient {
    pub fn new(
        runtime_handle: Handle,
        cluster_info: Arc<ClusterInfo>,
        poh_recorder: Arc<RwLock<PohRecorder>>,
        validator_identity: Option<&Keypair>,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(128);

        let cancel = CancellationToken::new();

        let leader_updater = ForwarderLeaderUpdater {
            cluster_info,
            poh_recorder,
        };
        let config = Self::create_config(validator_identity);
        let handle = runtime_handle.spawn(ConnectionWorkersScheduler::run(
            config,
            Box::new(leader_updater.clone()),
            receiver,
            cancel.clone(),
        ));

        Self {
            runtime_handle,
            join_and_cancel: Arc::new(Mutex::new((Some(handle), cancel))),
            sender,
            leader_updater,
        }
    }

    fn create_config(validator_identity: Option<&Keypair>) -> ConnectionWorkersSchedulerConfig {
        let client_certificate = QuicClientCertificate::with_option(validator_identity);
        ConnectionWorkersSchedulerConfig {
            bind: SocketAddr::new(Ipv4Addr::new(0, 0, 0, 0).into(), 0),
            client_certificate,
            // to match MAX_CONNECTIONS from ConnectionCache
            num_connections: 1024,
            skip_check_transaction_age: true,
            worker_channel_size: 64,
            max_reconnect_attempts: 4,
            leaders_fanout: Fanout {
                connect: 1,
                send: 1,
            },
        }
    }

    #[cfg(any(test, feature = "dev-context-only-utils"))]
    pub fn cancel(&self) -> Result<(), Box<dyn std::error::Error>> {
        let Ok(lock) = self.join_and_cancel.lock() else {
            return Err("Failed to stop scheduler: TpuClientNext task panicked.".into());
        };
        lock.1.cancel();
        Ok(())
    }

    async fn do_update_key(
        &self,
        validator_identity: &Keypair,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let runtime_handle = self.runtime_handle.clone();
        let config = Self::create_config(Some(validator_identity));
        let leader_updater = self.leader_updater.clone();
        let handle = self.join_and_cancel.clone();

        let Ok(mut lock) = handle.lock() else {
            return Err("TpuClientNext task panicked.".into());
        };
        lock.1.cancel();

        if let Some(join_handle) = lock.0.take() {
            // TODO probably wait with timeout? In case of error, can we return Receiver anyways?
            let Ok(result) = join_handle.await else {
                return Err("TpuClientNext task panicked.".into());
            };

            match result {
                Ok((_stats, receiver)) => {
                    let cancel = CancellationToken::new();
                    let handle = runtime_handle.spawn(ConnectionWorkersScheduler::run(
                        config,
                        Box::new(leader_updater),
                        receiver,
                        cancel.clone(),
                    ));

                    *lock = (Some(handle), cancel);
                }
                Err(error) => {
                    return Err(Box::new(error));
                }
            }
        }
        Ok(())
    }
}

impl NotifyKeyUpdate for ForwarderTpuClientNextClient {
    fn update_key(&self, validator_identity: &Keypair) -> Result<(), Box<dyn std::error::Error>> {
        self.runtime_handle
            .block_on(self.do_update_key(validator_identity))
    }
}

impl ForwarderClient for ForwarderTpuClientNextClient {
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
