use {
    crate::{banking_stage::LikeClusterInfo, next_leader::next_leader},
    async_trait::async_trait,
    lazy_static::lazy_static,
    solana_client::connection_cache::ConnectionCache,
    solana_gossip::contact_info::{ContactInfo, Protocol},
    solana_poh::poh_recorder::PohRecorder,
    solana_sdk::{
        pubkey::{self, Pubkey},
        transaction::SanitizedTransaction,
        transport::TransportError,
    },
    solana_tpu_client_next::{
        connection_workers_scheduler::ConnectionWorkersSchedulerConfig,
        leader_updater::LeaderUpdater, transaction_batch::TransactionBatch,
        ConnectionWorkersScheduler,
    },
    std::{
        net::{Ipv4Addr, SocketAddr},
        sync::{Arc, RwLock},
    },
    tokio::{
        runtime::Runtime,
        sync::mpsc::{self, Sender},
    },
    tokio_util::sync::CancellationToken,
};

#[derive(Clone)]
pub enum ForwardClient {
    ConnectionCache(Arc<ConnectionCache>),
    TpuClientNextSender(mpsc::Sender<TransactionBatch>),
}

impl From<Arc<ConnectionCache>> for ForwardClient {
    fn from(cache: Arc<ConnectionCache>) -> Self {
        ForwardClient::ConnectionCache(cache)
    }
}

lazy_static! {
    static ref RUNTIME: Runtime = tokio::runtime::Builder::new_multi_thread()
        .thread_name("solQuicClientRt")
        .enable_all()
        .build()
        .unwrap();
}
struct ForwardingStageLeaderUpdater<T: LikeClusterInfo> {
    pub poh_recorder: Arc<RwLock<PohRecorder>>,
    pub cluster_info: T,
}

#[async_trait]
impl<T: LikeClusterInfo> LeaderUpdater for ForwardingStageLeaderUpdater<T> {
    fn next_leaders(&self, _lookahead_slots: u64) -> Vec<SocketAddr> {
        let Some((_leader, address)) =
            next_leader(&self.cluster_info, &self.poh_recorder, |node| {
                ContactInfo::tpu_forwards(node, Protocol::QUIC)
            })
        else {
            return vec![];
        };
        vec![address]
    }

    async fn stop(&mut self) {}
}

pub(crate) fn spawn_tpu_client_next<T>(
    poh_recorder: Arc<RwLock<PohRecorder>>,
    cluster_info: T,
) -> Sender<TransactionBatch>
where
    T: LikeClusterInfo,
{
    let (transaction_sender, transaction_receiver) = mpsc::channel(16); // random number of now
    let validator_identity = None;
    RUNTIME.spawn(async move {
        let cancel = CancellationToken::new();
        let leader_updater = ForwardingStageLeaderUpdater {
            poh_recorder,
            cluster_info,
        };
        let config = ConnectionWorkersSchedulerConfig {
            bind: SocketAddr::new(Ipv4Addr::new(0, 0, 0, 0).into(), 0),
            stake_identity: validator_identity, // In CC, do we send with identity?
            num_connections: 1,
            skip_check_transaction_age: false,
            worker_channel_size: 2,
            max_reconnect_attempts: 4,
            lookahead_slots: 1,
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
