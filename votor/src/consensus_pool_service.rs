//! Service in charge of ingesting new messages into the certificate pool
//! and notifying votor of new events that occur
use {
    crate::{
        commitment::CommitmentAggregationData, event::VotorEventSender, voting_service::BLSOp,
    },
    crossbeam_channel::{Receiver, Sender},
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::{blockstore::Blockstore, leader_schedule_cache::LeaderScheduleCache},
    solana_pubkey::Pubkey,
    solana_runtime::bank_forks::SharableBanks,
    solana_votor_messages::consensus_message::ConsensusMessage,
    std::{
        sync::{atomic::AtomicBool, Arc, Condvar, Mutex},
        thread::JoinHandle,
    },
};

/// Inputs for the certificate pool thread
pub(crate) struct ConsensusPoolContext {
    pub(crate) exit: Arc<AtomicBool>,
    pub(crate) start: Arc<(Mutex<bool>, Condvar)>,

    pub(crate) cluster_info: Arc<ClusterInfo>,
    pub(crate) my_vote_pubkey: Pubkey,
    pub(crate) blockstore: Arc<Blockstore>,
    pub(crate) sharable_banks: SharableBanks,
    pub(crate) leader_schedule_cache: Arc<LeaderScheduleCache>,

    // TODO: for now we ingest our own votes into the certificate pool
    // just like regular votes. However do we need to convert
    // Vote -> BLSMessage -> Vote?
    // consider adding a separate pathway in consensus_pool.add_transaction for ingesting own votes
    pub(crate) consensus_message_receiver: Receiver<ConsensusMessage>,

    pub(crate) bls_sender: Sender<BLSOp>,
    pub(crate) event_sender: VotorEventSender,
    pub(crate) commitment_sender: Sender<CommitmentAggregationData>,
}

pub(crate) struct ConsensusPoolService {
    t_ingest: JoinHandle<()>,
}
