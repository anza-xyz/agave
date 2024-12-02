//! Client code that allows choosing the client for forwarding by wrapping
//! tpu-client-next and ConnectionCache.
// TODO Should it be inside banking stage?
use {
    crate::{banking_stage::LikeClusterInfo, next_leader::next_leader},
    async_trait::async_trait,
    log::warn,
    solana_client::connection_cache::{ConnectionCache, Protocol},
    solana_connection_cache::client_connection::ClientConnection as TpuConnection,
    solana_poh::poh_recorder::PohRecorder,
    solana_sdk::{pubkey::Pubkey, quic::NotifyKeyUpdate, signature::Keypair},
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

pub(crate) trait ForwarderClient: Send + Clone + 'static {
    fn send_transaction_batch(&self, wire_transactions: Vec<Vec<u8>>);

    fn get_next_leader(&self) -> Option<(Pubkey, SocketAddr)>;
    fn get_next_leader_vote(&self) -> Option<(Pubkey, SocketAddr)>;
}

#[derive(Clone)]
pub(crate) struct ForwarderConnectionCacheClient<T: LikeClusterInfo> {
    //TODo why Arc? ConnectionCache is Arc inside
    connection_cache: Arc<ConnectionCache>,
    leader_updater: ForwarderLeaderUpdater<T>,
}

impl<T: LikeClusterInfo> ForwarderConnectionCacheClient<T> {
    pub fn new(
        connection_cache: Arc<ConnectionCache>,
        leader_updater: ForwarderLeaderUpdater<T>,
    ) -> Self {
        Self {
            connection_cache,
            leader_updater,
        }
    }
}

impl<T: LikeClusterInfo> ForwarderClient for ForwarderConnectionCacheClient<T> {
    fn send_transaction_batch(&self, wire_transactions: Vec<Vec<u8>>) {
        let address = self.leader_updater.get_next_leader();
        let Some((_pubkey, address)) = address else {
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

    fn get_next_leader(&self) -> Option<(Pubkey, SocketAddr)> {
        self.leader_updater.get_next_leader()
    }

    fn get_next_leader_vote(&self) -> Option<(Pubkey, SocketAddr)> {
        self.leader_updater.get_next_leader_vote()
    }
}

impl<T: LikeClusterInfo> NotifyKeyUpdate for ForwarderConnectionCacheClient<T> {
    fn update_key(&self, validator_identity: &Keypair) -> Result<(), Box<dyn std::error::Error>> {
        self.connection_cache.update_key(validator_identity)
    }
}

#[derive(Clone)]
pub(crate) struct ForwarderLeaderUpdater<T: LikeClusterInfo> {
    cluster_info: T,
    poh_recorder: Arc<RwLock<PohRecorder>>,
}

impl<T: LikeClusterInfo> ForwarderLeaderUpdater<T> {
    pub fn new(cluster_info: T, poh_recorder: Arc<RwLock<PohRecorder>>) -> Self {
        Self {
            cluster_info,
            poh_recorder,
        }
    }

    pub fn get_next_leader(&self) -> Option<(Pubkey, SocketAddr)> {
        next_leader(&self.cluster_info, &self.poh_recorder, |node| {
            node.tpu_forwards(Protocol::QUIC)
        })
    }

    pub fn get_next_leader_vote(&self) -> Option<(Pubkey, SocketAddr)> {
        next_leader(&self.cluster_info, &self.poh_recorder, |node| {
            node.tpu_vote(Protocol::UDP)
        })
    }
}

#[async_trait]
impl<T: LikeClusterInfo> LeaderUpdater for ForwarderLeaderUpdater<T> {
    fn next_leaders(&mut self, _lookahead_leaders: usize) -> Vec<SocketAddr> {
        let Some((_pubkey, address)) = self.get_next_leader() else {
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
pub struct ForwarderTpuClientNextClient<T: LikeClusterInfo> {
    runtime_handle: Handle,
    sender: mpsc::Sender<TransactionBatch>,
    // This handle is needed to implement `NotifyKeyUpdate` trait. It's only
    // method takes &self and thus we need to wrap with Mutex
    join_and_cancel: Arc<Mutex<(Option<TpuClientJoinHandle>, CancellationToken)>>,
    leader_updater: ForwarderLeaderUpdater<T>,
}

impl<T: LikeClusterInfo> ForwarderTpuClientNextClient<T> {
    pub fn new(
        runtime_handle: Handle,
        leader_updater: ForwarderLeaderUpdater<T>,
        validator_identity: Option<&Keypair>,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(128);

        let cancel = CancellationToken::new();

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

impl<T: LikeClusterInfo> NotifyKeyUpdate for ForwarderTpuClientNextClient<T> {
    fn update_key(&self, validator_identity: &Keypair) -> Result<(), Box<dyn std::error::Error>> {
        self.runtime_handle
            .block_on(self.do_update_key(validator_identity))
    }
}

impl<T: LikeClusterInfo> ForwarderClient for ForwarderTpuClientNextClient<T> {
    fn send_transaction_batch(&self, wire_transactions: Vec<Vec<u8>>) {
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

    fn get_next_leader(&self) -> Option<(Pubkey, SocketAddr)> {
        self.leader_updater.get_next_leader()
    }

    fn get_next_leader_vote(&self) -> Option<(Pubkey, SocketAddr)> {
        self.leader_updater.get_next_leader_vote()
    }
}
