use {
    crate::{
        consensus::tower_storage::{SavedTowerVersions, TowerStorage},
        next_leader::upcoming_leader_tpu_vote_sockets,
    },
    bincode::serialize,
    crossbeam_channel::Receiver,
    solana_client::connection_cache::ConnectionCache,
    solana_clock::{Slot, FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET},
    solana_connection_cache::client_connection::ClientConnection,
    solana_gossip::cluster_info::ClusterInfo,
    solana_measure::measure::Measure,
    solana_poh::poh_recorder::PohRecorder,
    solana_transaction::Transaction,
    solana_transaction_error::TransportError,
    std::{
        net::SocketAddr,
        sync::{Arc, RwLock},
        thread::{self, Builder, JoinHandle},
    },
    thiserror::Error,
};

pub trait VoteTransportClient: Send + Sync {
    fn send_vote(
        &self,
        cluster_info: &ClusterInfo,
        poh_recorder: &RwLock<PohRecorder>,
        transaction: &Transaction,
    );
}

impl VoteTransportClient for ConnectionCache {
    fn send_vote(
        &self,
        cluster_info: &ClusterInfo,
        poh_recorder: &RwLock<PohRecorder>,
        transaction: &Transaction,
    ) {
        const UPCOMING_LEADER_FANOUT_SLOTS: u64 =
            FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET.saturating_add(1);
        #[cfg(test)]
        static_assertions::const_assert_eq!(UPCOMING_LEADER_FANOUT_SLOTS, 3);
        let upcoming_leader_sockets = upcoming_leader_tpu_vote_sockets(
            cluster_info,
            poh_recorder,
            UPCOMING_LEADER_FANOUT_SLOTS,
            self.protocol(),
        );

        if !upcoming_leader_sockets.is_empty() {
            for tpu_vote_socket in upcoming_leader_sockets {
                let _ =
                    send_vote_transaction(cluster_info, transaction, Some(tpu_vote_socket), self);
            }
        } else {
            // Send to our own tpu vote socket if we cannot find a leader to send to
            let _ = send_vote_transaction(cluster_info, transaction, None, self);
        }
    }
}

#[cfg(feature = "dev-context-only-utils")]
pub struct TpuClientNextVoteTransport {
    sender: solana_tpu_client_next::TransactionSender,
    cancel: tokio_util::sync::CancellationToken,
    _client: solana_tpu_client_next::Client,
    _runtime: tokio::runtime::Runtime,
}

#[cfg(feature = "dev-context-only-utils")]
impl Default for TpuClientNextVoteTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "dev-context-only-utils")]
impl TpuClientNextVoteTransport {
    pub fn new() -> Self {
        use {
            solana_tpu_client_next::{leader_updater::create_pinned_leader_updater, ClientBuilder},
            std::net::{IpAddr, Ipv4Addr},
            tokio_util::sync::CancellationToken,
        };

        let cancel = CancellationToken::new();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime for vote transport");

        let bind_socket = solana_net_utils::sockets::bind_to(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
            .expect("Failed to bind vote transport socket");

        let leader_updater =
            create_pinned_leader_updater(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0));

        let (sender, client) = ClientBuilder::new(leader_updater)
            .runtime_handle(runtime.handle().clone())
            .cancel_token(cancel.clone())
            .bind_socket(bind_socket)
            .leader_send_fanout(1)
            .max_cache_size(1)
            .build()
            .expect("Failed to build tpu-client-next for vote transport");

        Self {
            sender,
            cancel,
            _client: client,
            _runtime: runtime,
        }
    }
}

#[cfg(feature = "dev-context-only-utils")]
impl Drop for TpuClientNextVoteTransport {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[cfg(feature = "dev-context-only-utils")]
impl VoteTransportClient for TpuClientNextVoteTransport {
    fn send_vote(
        &self,
        _cluster_info: &ClusterInfo,
        _poh_recorder: &RwLock<PohRecorder>,
        transaction: &Transaction,
    ) {
        let buf = serialize(transaction).expect("vote serialization failed");
        let _ = self.sender.try_send_transactions_in_batch(vec![buf]);
    }
}

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

#[derive(Debug, Error)]
enum SendVoteError {
    #[error(transparent)]
    BincodeError(#[from] bincode::Error),
    #[error("Invalid TPU address")]
    InvalidTpuAddress,
    #[error(transparent)]
    TransportError(#[from] TransportError),
}

fn send_vote_transaction(
    cluster_info: &ClusterInfo,
    transaction: &Transaction,
    tpu: Option<SocketAddr>,
    connection_cache: &ConnectionCache,
) -> Result<(), SendVoteError> {
    let tpu = tpu
        .or_else(|| {
            cluster_info
                .my_contact_info()
                .tpu(connection_cache.protocol())
        })
        .ok_or(SendVoteError::InvalidTpuAddress)?;
    let buf = Arc::new(serialize(transaction)?);
    let client = connection_cache.get_connection(&tpu);

    client.send_data_async(buf).map_err(|err| {
        error!("Ran into an error when sending vote: {err:?} to {tpu:?}");
        SendVoteError::from(err)
    })
}

pub struct VotingService {
    thread_hdl: JoinHandle<()>,
}

impl VotingService {
    pub fn new(
        vote_receiver: Receiver<VoteOp>,
        cluster_info: Arc<ClusterInfo>,
        poh_recorder: Arc<RwLock<PohRecorder>>,
        tower_storage: Arc<dyn TowerStorage>,
        vote_client: Arc<dyn VoteTransportClient>,
    ) -> Self {
        let thread_hdl = Builder::new()
            .name("solVoteService".to_string())
            .spawn({
                move || {
                    for vote_op in vote_receiver.iter() {
                        Self::handle_vote(
                            &cluster_info,
                            &poh_recorder,
                            tower_storage.as_ref(),
                            vote_op,
                            vote_client.as_ref(),
                        );
                    }
                }
            })
            .unwrap();
        Self { thread_hdl }
    }

    pub fn handle_vote(
        cluster_info: &ClusterInfo,
        poh_recorder: &RwLock<PohRecorder>,
        tower_storage: &dyn TowerStorage,
        vote_op: VoteOp,
        vote_client: &dyn VoteTransportClient,
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

        // Send our vote transaction to the leaders for the next few slots
        vote_client.send_vote(cluster_info, poh_recorder, vote_op.tx());

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
