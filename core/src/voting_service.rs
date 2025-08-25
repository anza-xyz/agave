use {
    crate::{
        consensus::tower_storage::{SavedTowerVersions, TowerStorage},
        mock_alpenglow_consensus::MockAlpenglowConsensus,
        next_leader::upcoming_leader_tpu_vote_sockets,
    },
    bincode::serialize,
    crossbeam_channel::Receiver,
    solana_client::connection_cache::ConnectionCache,
    solana_clock::{Slot, FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET},
    solana_connection_cache::client_connection::ClientConnection,
    solana_gossip::{cluster_info::ClusterInfo, epoch_specs::EpochSpecs},
    solana_measure::measure::Measure,
    solana_poh::poh_recorder::PohRecorder,
    solana_runtime::bank_forks::BankForks,
    solana_transaction::Transaction,
    solana_transaction_error::TransportError,
    std::{
        net::{SocketAddr, UdpSocket},
        sync::{Arc, RwLock},
        thread::{self, Builder, JoinHandle},
    },
    thiserror::Error,
};

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
        trace!("Ran into an error when sending vote: {err:?} to {tpu:?}");
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
        connection_cache: Arc<ConnectionCache>,
        alpenglow_socket: Option<UdpSocket>,
        bank_forks: Arc<RwLock<BankForks>>,
    ) -> Self {
        let thread_hdl = Builder::new()
            .name("solVoteService".to_string())
            .spawn({
                let mut mock_alpenglow = alpenglow_socket.map(|s| {
                    MockAlpenglowConsensus::new(
                        s,
                        cluster_info.clone(),
                        EpochSpecs::from(bank_forks.clone()),
                    )
                });
                move || {
                    for vote_op in vote_receiver.iter() {
                        // Figure out if we are casting a vote for a new slot, and what slot it is for
                        let vote_slot = match vote_op {
                            VoteOp::PushVote {
                                tx: _,
                                ref tower_slots,
                                ..
                            } => tower_slots.iter().copied().last(),
                            _ => None,
                        };
                        // perform all the normal vote handling routines
                        Self::handle_vote(
                            &cluster_info,
                            &poh_recorder,
                            tower_storage.as_ref(),
                            vote_op,
                            connection_cache.clone(),
                        );
                        // trigger mock alpenglow vote if we have just cast an actual vote
                        if let Some(slot) = vote_slot {
                            if let Some(ag) = mock_alpenglow.as_mut() {
                                let root_bank = { bank_forks.read().unwrap().root_bank() };
                                ag.signal_new_slot(slot, &root_bank);
                            }
                        }
                    }
                    if let Some(ag) = mock_alpenglow {
                        let _ = ag.join();
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
        connection_cache: Arc<ConnectionCache>,
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

        // Attempt to send our vote transaction to the leaders for the next few
        // slots. From the current slot to the forwarding slot offset
        // (inclusive).
        const UPCOMING_LEADER_FANOUT_SLOTS: u64 =
            FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET.saturating_add(1);
        #[cfg(test)]
        static_assertions::const_assert_eq!(UPCOMING_LEADER_FANOUT_SLOTS, 3);
        let upcoming_leader_sockets = upcoming_leader_tpu_vote_sockets(
            cluster_info,
            poh_recorder,
            UPCOMING_LEADER_FANOUT_SLOTS,
            connection_cache.protocol(),
        );

        if !upcoming_leader_sockets.is_empty() {
            for tpu_vote_socket in upcoming_leader_sockets {
                let _ = send_vote_transaction(
                    cluster_info,
                    vote_op.tx(),
                    Some(tpu_vote_socket),
                    &connection_cache,
                );
            }
        } else {
            // Send to our own tpu vote socket if we cannot find a leader to send to
            let _ = send_vote_transaction(cluster_info, vote_op.tx(), None, &connection_cache);
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
