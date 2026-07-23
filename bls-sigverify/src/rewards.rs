use {
    agave_votor_messages::{
        consensus_message::VoteMessage, reward_certificate::NUM_SLOTS_FOR_REWARD,
        sig_verified_messages::VoteAggregate, vote::Vote,
    },
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
};

#[allow(clippy::large_enum_variant)]
pub enum RewardInput {
    External(Vec<VoteAggregate>),
    Own(VoteMessage),
}

#[must_use]
/// Returns true if the given `msg` is needed for rewards.
pub fn rewards_wants_vote(
    cluster_info: &ClusterInfo,
    leader_schedule: &LeaderScheduleCache,
    root_slot: Slot,
    vote: &Vote,
) -> bool {
    match vote {
        Vote::Finalize(_)
        | Vote::NotarizeFallback(_)
        | Vote::SkipFallback(_)
        | Vote::Genesis(_) => return false,
        Vote::Notarize(_) | Vote::Skip(_) => (),
    }
    let vote_slot = vote.slot();
    vote_is_relevant_for_rewards(vote_slot, root_slot, cluster_info, leader_schedule)
}

#[must_use]
/// Returns true if a reward vote at the `vote_slot` is needed by this node for rewards.
pub fn vote_is_relevant_for_rewards(
    vote_slot: Slot,
    root_slot: Slot,
    cluster_info: &ClusterInfo,
    leader_schedule: &LeaderScheduleCache,
) -> bool {
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
