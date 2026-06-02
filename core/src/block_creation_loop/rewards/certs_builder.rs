use {
    crate::block_creation_loop::rewards::msg_types::{
        RewardRequest, RewardRespSucc, RewardResponse,
    },
    agave_votor_messages::{
        consensus_message::VoteMessage,
        reward_certificate::{BuildRewardCertsRespError, NUM_SLOTS_FOR_REWARD},
        vote::Vote,
    },
    crossbeam_channel::RecvError,
    entry::Entry,
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_runtime::bank::Bank,
    std::{collections::BTreeMap, sync::Arc},
};

mod entry;

/// Returns [`false`] if the rewards container is not interested in the [`VoteMessage`].
/// Returns [`true`] if the rewards container might be interested in the [`VoteMessage`].
pub fn wants_vote(
    cluster_info: &ClusterInfo,
    leader_schedule: &LeaderScheduleCache,
    root_slot: Slot,
    vote: &VoteMessage,
) -> bool {
    match vote.vote {
        Vote::Notarize(_) | Vote::Skip(_) => (),
        Vote::Finalize(_)
        | Vote::NotarizeFallback(_)
        | Vote::SkipFallback(_)
        | Vote::Genesis(_) => return false,
    }
    let vote_slot = vote.vote.slot();
    if vote_slot.saturating_add(NUM_SLOTS_FOR_REWARD) <= root_slot {
        return false;
    }
    let my_pubkey = cluster_info.id();
    let Some(leader) =
        leader_schedule.slot_leader_at(vote_slot.saturating_add(NUM_SLOTS_FOR_REWARD), None)
    else {
        return false;
    };
    if leader.id != my_pubkey {
        return false;
    }
    true
}

/// Container to store state needed to generate reward certificates.
pub(super) struct CertsBuilder {
    /// Per [`Slot`], stores skip and notar votes.
    votes: BTreeMap<Slot, Entry>,
    /// Stores the latest pubkey for the current node.
    cluster_info: Arc<ClusterInfo>,
    /// Stores the leader schedules.
    leader_schedule: Arc<LeaderScheduleCache>,
}

impl CertsBuilder {
    /// Constructs a new instance of [`CertsBuilder`].
    pub(super) fn new(
        cluster_info: Arc<ClusterInfo>,
        leader_schedule: Arc<LeaderScheduleCache>,
    ) -> Self {
        Self {
            votes: BTreeMap::default(),
            cluster_info,
            leader_schedule,
        }
    }

    /// Builds reward certificates.
    fn build_certs(
        &mut self,
        bank_slot: Slot,
    ) -> Result<RewardRespSucc, BuildRewardCertsRespError> {
        let Some(reward_slot) = bank_slot.checked_sub(NUM_SLOTS_FOR_REWARD) else {
            return Ok(RewardRespSucc::default());
        };
        // we assume that the block creation loop will only ever request to build reward certs in a
        // strictly increasing order so we can drop older state
        self.votes = self.votes.split_off(&reward_slot);
        match self.votes.remove(&reward_slot) {
            None => Ok(RewardRespSucc::default()),
            Some(entry) => entry.build_certs(reward_slot),
        }
    }

    pub(super) fn build_request(
        &mut self,
        request: Result<RewardRequest, RecvError>,
    ) -> Result<(), ()> {
        let my_pubkey = self.cluster_info.id();
        match request {
            Ok(RewardRequest {
                bank_slot,
                reply_sender,
            }) => {
                let resp = RewardResponse {
                    result: self.build_certs(bank_slot),
                };
                let _ = reply_sender.send(resp).inspect_err(|_| {
                    info!(
                        "{my_pubkey}: channel to send reply for bank_slot={bank_slot} disconnected"
                    );
                });
                Ok(())
            }
            Err(_) => {
                error!("{my_pubkey}: build reward certs channel is disconnected; exiting.");
                Err(())
            }
        }
    }

    /// Returns [`true`] if the rewards container is interested in this vote else [`false`].
    fn wants_vote(&self, root_slot: Slot, vote: &VoteMessage) -> bool {
        if !wants_vote(&self.cluster_info, &self.leader_schedule, root_slot, vote) {
            return false;
        }
        let Some(entry) = self.votes.get(&vote.vote.slot()) else {
            return true;
        };
        entry.wants_vote(vote)
    }

    /// Adds received [`VoteMessage`] from other validators.
    pub(super) fn add_vote(&mut self, root_bank: &Bank, vote: &VoteMessage) {
        let slot = vote.vote.slot();
        let Some(rank_map) = root_bank.get_rank_map(slot) else {
            warn!(
                "failed to look up rank_map for slot {slot} using bank for slot {}",
                root_bank.slot()
            );
            return;
        };
        let max_validators = rank_map.len();
        let root_slot = root_bank.slot();
        // drop state that is too old based on how the root slot has progressed
        // TODO: if this actually purges state, that probably indicates that the leader missed its
        // window.  We should have a metric for this.
        self.votes = self
            .votes
            .split_off(&root_slot.saturating_sub(NUM_SLOTS_FOR_REWARD));

        if !self.wants_vote(root_slot, vote) {
            return;
        }
        match self
            .votes
            .entry(vote.vote.slot())
            .or_insert(Entry::new(max_validators))
            .add_vote(rank_map, vote)
        {
            Ok(()) => (),
            Err(e) => {
                warn!("Adding vote {vote:?} failed with {e}");
            }
        }
    }
}
