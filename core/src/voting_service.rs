use {
    crate::{
        consensus::tower_storage::{SavedTowerVersions, TowerStorage},
        next_leader::upcoming_leader_tpu_vote_sockets,
    },
    bincode::serialize,
    crossbeam_channel::Receiver,
    solana_client::connection_cache::ConnectionCache,
    solana_clock::{Slot, FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET},
    solana_connection_cache::{client_connection::ClientConnection, connection_cache::Protocol},
    solana_gossip::cluster_info::ClusterInfo,
    solana_measure::measure::Measure,
    solana_poh::poh_recorder::PohRecorder,
    solana_tpu_client_next::TransactionSender,
    solana_transaction::Transaction,
    solana_transaction_error::TransportError,
    std::{
        net::SocketAddr,
        sync::{Arc, RwLock},
        thread::{self, Builder, JoinHandle},
    },
    thiserror::Error,
};

pub(crate) const QUIC_UPCOMING_LEADER_FANOUT_LEADERS: usize = 2;

// Attempt to send our vote transaction to the leaders for the next few
// slots. From the current slot to the forwarding slot offset
// (inclusive).
const UDP_UPCOMING_LEADER_FANOUT_SLOTS: u64 =
    FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET.saturating_add(1);
#[cfg(test)]
static_assertions::const_assert_eq!(UDP_UPCOMING_LEADER_FANOUT_SLOTS, 3);

pub struct QuicVoteSender(pub TransactionSender);

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
    connection_cache: &Arc<ConnectionCache>,
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
        udp_connection_cache: Arc<ConnectionCache>,
        quic_sender: Option<QuicVoteSender>,
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
                            &udp_connection_cache,
                            &quic_sender,
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
        udp_connection_cache: &Arc<ConnectionCache>,
        quic_sender: &Option<QuicVoteSender>,
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

        let udp_upcoming_leader_sockets = upcoming_leader_tpu_vote_sockets(
            cluster_info,
            poh_recorder,
            UDP_UPCOMING_LEADER_FANOUT_SLOTS,
            Protocol::UDP,
        );

        if !udp_upcoming_leader_sockets.is_empty() {
            for tpu_vote_socket in udp_upcoming_leader_sockets {
                let _ = send_vote_transaction(
                    cluster_info,
                    vote_op.tx(),
                    Some(tpu_vote_socket),
                    udp_connection_cache,
                );
            }
        } else {
            // Send to our own tpu vote socket if we cannot find a leader to send to
            let _ = send_vote_transaction(cluster_info, vote_op.tx(), None, udp_connection_cache);
        }

        if let Some(quic_sender) = quic_sender {
            if let Ok(serialized) = serialize(vote_op.tx()) {
                if let Err(e) = quic_sender
                    .0
                    .try_send_transactions_in_batch(vec![serialized])
                {
                    warn!("Error sending vote transaction with QUIC: {e}");
                }
            } else {
                warn!("Failed to serialize vote");
            }
        }

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
