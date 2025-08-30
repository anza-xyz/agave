use {
    crate::{
        consensus::tower_storage::{SavedTowerVersions, TowerStorage},
        mock_alpenglow_consensus::MockAlpenglowConsensus,
        tvu::VoteClientOption,
        vote_client::{ClusterTpuInfo, ConnectionCacheClient, TpuClientNextVoteClient, VoteClient},
    },
    bincode::serialize,
    crossbeam_channel::Receiver,
    solana_clock::{Slot, FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET},
    solana_gossip::{cluster_info::ClusterInfo, contact_info::Protocol, epoch_specs::EpochSpecs},
    solana_measure::measure::Measure,
    solana_poh::poh_recorder::PohRecorder,
    solana_runtime::bank_forks::BankForks,
    solana_transaction::Transaction,
    solana_transaction_error::TransportError,
    std::{
        net::UdpSocket,
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
    #[error(transparent)]
    TransportError(#[from] TransportError),
}

// Helper for sending vote transaction with generic VoteClient
fn send_vote_transaction<V: VoteClient + ?Sized>(
    transaction: &Transaction,
    vote_client: &V,
) -> Result<(), SendVoteError> {
    let buf = serialize(transaction)?;
    vote_client.send_transactions_in_batch(vec![buf]);
    Ok(())
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
        vote_client: VoteClientOption,
        alpenglow_socket: Option<UdpSocket>,
        bank_forks: Arc<RwLock<BankForks>>,
    ) -> Self {
        // Attempt to send our vote transaction to the leaders for the next few
        // slots. From the current slot to the forwarding slot offset
        // (inclusive).
        const UPCOMING_LEADER_FANOUT_SLOTS: u64 =
            FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET.saturating_add(1);
        #[cfg(test)]
        static_assertions::const_assert_eq!(UPCOMING_LEADER_FANOUT_SLOTS, 3);

        // Pattern match and construct the concrete vote client type
        match vote_client {
            VoteClientOption::ConnectionCache(connection_cache) => {
                let my_tpu_address = cluster_info.my_contact_info().tpu_vote(Protocol::QUIC)
                    .unwrap_or_else(|| {
                        panic!("Vote client requires a valid TPU address, but none found in cluster info");
                    });
                let leader_info = Some(ClusterTpuInfo::new(
                    cluster_info.clone(),
                    poh_recorder.clone(),
                ));

                let vote_client = ConnectionCacheClient::new(
                    connection_cache,
                    my_tpu_address,
                    leader_info,
                    UPCOMING_LEADER_FANOUT_SLOTS,
                );

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
                        let cluster_info = cluster_info.clone();
                        let tower_storage = tower_storage.clone();
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
                                Self::handle_vote(
                                    &cluster_info,
                                    tower_storage.as_ref(),
                                    vote_op,
                                    &vote_client,
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
            VoteClientOption::TpuClientNext(
                identity_keypair,
                vote_client_socket,
                client_runtime,
                cancel,
            ) => {
                let my_tpu_address = cluster_info.my_contact_info().tpu_vote(Protocol::QUIC)
                    .unwrap_or_else(|| {
                        panic!("Vote client requires a valid TPU address, but none found in cluster info");
                    });
                let leader_info = Some(ClusterTpuInfo::new(
                    cluster_info.clone(),
                    poh_recorder.clone(),
                ));

                let vote_client = TpuClientNextVoteClient::new(
                    client_runtime,
                    my_tpu_address,
                    leader_info,
                    Some(identity_keypair),
                    vote_client_socket,
                    UPCOMING_LEADER_FANOUT_SLOTS,
                    cancel,
                );

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
                        {
                            let cluster_info = cluster_info.clone();
                            let tower_storage = tower_storage.clone();
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
                                        tower_storage.as_ref(),
                                        vote_op,
                                        &vote_client,
                                    );
                                    // trigger mock alpenglow vote if we have just cast an actual vote
                                    if let Some(slot) = vote_slot {
                                        if let Some(ag) = mock_alpenglow.as_mut() {
                                            let root_bank =
                                                { bank_forks.read().unwrap().root_bank() };
                                            ag.signal_new_slot(slot, &root_bank);
                                        }
                                    }
                                }
                                if let Some(ag) = mock_alpenglow {
                                    let _ = ag.join();
                                }
                            }
                        }
                    })
                    .unwrap();
                Self { thread_hdl }
            }
        }
    }

    // Generic version of handle_vote
    pub fn handle_vote<V: VoteClient + Send + Sync>(
        cluster_info: &ClusterInfo,
        tower_storage: &dyn TowerStorage,
        vote_op: VoteOp,
        vote_client: &V,
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

        let _ = send_vote_transaction(vote_op.tx(), vote_client);

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
