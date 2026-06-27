use {
    crate::sig_verified_messages::VoteAggregate,
    agave_votor_messages::{reward_certificate::NUM_SLOTS_FOR_REWARD, vote::Vote},
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
};

/// Returns [`false`] if the rewards container is not interested in the [`VoteMessage`].
/// Returns [`true`] if the rewards container might be interested in the [`VoteMessage`].
pub fn rewards_wants_vote(
    cluster_info: &ClusterInfo,
    leader_schedule: &LeaderScheduleCache,
    root_slot: Slot,
    vote: &Vote,
) -> bool {
    match vote {
        Vote::Notarize(_) | Vote::Skip(_) => (),
        Vote::Finalize(_)
        | Vote::NotarizeFallback(_)
        | Vote::SkipFallback(_)
        | Vote::Genesis(_) => return false,
    }
    let vote_slot = vote.slot();
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

/// Returns true if `Vote` can be pruned from the received_votes cache.
pub(crate) fn should_prune_vote(vote: &Vote, root_slot: Slot) -> bool {
    let slot = vote.slot();
    match vote {
        Vote::Notarize(_) | Vote::Skip(_) => slot.saturating_add(NUM_SLOTS_FOR_REWARD) <= root_slot,
        Vote::Finalize(_)
        | Vote::NotarizeFallback(_)
        | Vote::SkipFallback(_)
        | Vote::Genesis(_) => slot < root_slot,
    }
}

/// Message to add votes to the rewards container.
#[derive(Debug)]
pub struct AddVoteMessage {
    pub votes: Vec<VoteAggregate>,
}
