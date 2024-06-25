use {
    crate::{
        consensus::tower_storage::{SavedTowerVersions, TowerStorage},
        next_leader::{next_leader_tpu_vote, next_vote_slot_leader_tpu_vote},
    },
    crossbeam_channel::Receiver,
    lazy_static::lazy_static,
    solana_gossip::{cluster_info::ClusterInfo, gossip_error::GossipError},
    solana_measure::measure::Measure,
    solana_poh::poh_recorder::PohRecorder,
    solana_sdk::{clock::Slot, pubkey::Pubkey, transaction::Transaction},
    std::{
        net::SocketAddr,
        sync::{Arc, RwLock},
        thread::{self, Builder, JoinHandle},
    },
    strum::VariantNames,
    strum_macros::{Display, EnumString, EnumVariantNames, IntoStaticStr},
};

#[derive(Clone, Copy, EnumString, EnumVariantNames, Default, IntoStaticStr, Display)]
#[strum(serialize_all = "kebab-case")]
pub enum VoteTxLeaderSelectionMethod {
    #[default]
    PohRecorder,
    VoteSlot,
    Both,
}

impl VoteTxLeaderSelectionMethod {
    pub const fn cli_names() -> &'static [&'static str] {
        Self::VARIANTS
    }

    pub fn cli_message() -> &'static str {
        lazy_static! {
            static ref MESSAGE: String = format!(
                "Choose the source of truth to determine which leader to send a vote to [default: {}]",
                VoteTxLeaderSelectionMethod::default()
            );
        };

        &MESSAGE
    }

    pub(crate) fn select_leaders(
        &self,
        vote_op: &VoteOp,
        cluster_info: &ClusterInfo,
        poh_recorder: &RwLock<PohRecorder>,
    ) -> Vec<Option<(Pubkey, SocketAddr)>> {
        let vote_slot = vote_op
            .last_voted_slot()
            .expect("Should not send an empty vote");
        match (self, vote_op.is_refresh_vote()) {
            (_, true) | (VoteTxLeaderSelectionMethod::PohRecorder, _) => {
                // Refresh votes should always select leaders based on PoH, as the vote slot is outdated
                vec![next_leader_tpu_vote(cluster_info, poh_recorder)]
            }
            (VoteTxLeaderSelectionMethod::VoteSlot, _) => {
                // Send to the leader of vote_slot + 1
                vec![next_vote_slot_leader_tpu_vote(
                    vote_slot,
                    cluster_info,
                    poh_recorder,
                )]
            }
            (VoteTxLeaderSelectionMethod::Both, _) => {
                // Send to both the PoH leader, and the leader of vote_slot + 1
                vec![
                    next_vote_slot_leader_tpu_vote(vote_slot, cluster_info, poh_recorder),
                    next_leader_tpu_vote(cluster_info, poh_recorder),
                ]
            }
        }
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

    fn last_voted_slot(&self) -> Option<Slot> {
        match self {
            VoteOp::PushVote { tower_slots, .. } => tower_slots.last().copied(),
            VoteOp::RefreshVote {
                last_voted_slot, ..
            } => Some(*last_voted_slot),
        }
    }

    fn is_refresh_vote(&self) -> bool {
        matches!(self, VoteOp::RefreshVote { .. })
    }
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
        leader_selection_method: VoteTxLeaderSelectionMethod,
    ) -> Self {
        info!(
            "Starting voting service with leader selection method {}",
            leader_selection_method
        );
        let thread_hdl = Builder::new()
            .name("solVoteService".to_string())
            .spawn(move || {
                for vote_op in vote_receiver.iter() {
                    Self::handle_vote(
                        &cluster_info,
                        &poh_recorder,
                        tower_storage.as_ref(),
                        vote_op,
                        leader_selection_method,
                    );
                }
            })
            .unwrap();
        Self { thread_hdl }
    }

    fn push_vote(
        vote_op: &VoteOp,
        cluster_info: &ClusterInfo,
        poh_recorder: &RwLock<PohRecorder>,
        leader_selection_method: VoteTxLeaderSelectionMethod,
    ) -> Result<(), GossipError> {
        leader_selection_method
            .select_leaders(vote_op, cluster_info, poh_recorder)
            .into_iter()
            .try_for_each(|leader| {
                cluster_info.send_transaction(
                    vote_op.tx(),
                    leader.map(|(_pubkey, target_addr)| target_addr),
                )
            })
    }

    pub fn handle_vote(
        cluster_info: &ClusterInfo,
        poh_recorder: &RwLock<PohRecorder>,
        tower_storage: &dyn TowerStorage,
        vote_op: VoteOp,
        leader_selection_method: VoteTxLeaderSelectionMethod,
    ) {
        if let VoteOp::PushVote { saved_tower, .. } = &vote_op {
            let mut measure = Measure::start("tower_save-ms");
            if let Err(err) = tower_storage.store(saved_tower) {
                error!("Unable to save tower to storage: {:?}", err);
                std::process::exit(1);
            }
            measure.stop();
            inc_new_counter_info!("tower_save-ms", measure.as_ms() as usize);
        }

        if let Err(e) = Self::push_vote(
            &vote_op,
            cluster_info,
            poh_recorder,
            leader_selection_method,
        ) {
            warn!(
                "Failed to send vote tx for {:?}: {}",
                vote_op.last_voted_slot(),
                e
            );
            datapoint_warn!(
                "voting-service-send-failure",
                ("vote_slot", vote_op.last_voted_slot(), Option<i64>),
            );
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
