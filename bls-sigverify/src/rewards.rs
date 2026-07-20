use {
    agave_votor_messages::{
        consensus_message::VoteMessage,
        reward_certificate::NUM_SLOTS_FOR_REWARD,
        unverified_vote_message::UnverifiedVoteMessage,
        vote::{NotarizationVote, SkipVote, Vote},
    },
    solana_bls_signatures::Signature as BLSSignature,
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_pubkey::Pubkey,
    std::num::NonZero,
};

#[derive(Debug, Clone)]
/// Different types of reward votes.
pub enum RewardVote {
    /// Notar type reward vote.
    Notar(NotarizationVote),
    /// Skip type reward vote.
    Skip(SkipVote),
}

impl RewardVote {
    /// Returns the slot on the vote
    pub fn slot(&self) -> Slot {
        match self {
            Self::Notar(v) => v.block.slot,
            Self::Skip(v) => v.slot,
        }
    }
}

#[derive(Debug, Clone)]
/// A reward vote message.
pub struct RewardVoteMessage {
    /// The type of reward vote.
    pub vote: RewardVote,
    /// The signature on the vote.
    pub signature: BLSSignature,
    /// The rank of the validator.
    pub rank: u16,
    pub stake: NonZero<u64>,
    pub vote_account_pubkey: Pubkey,
}

impl RewardVoteMessage {
    /// Returns a new RewardVoteMesage if the `msg` is needed for rewards to this node and the
    /// `msg.vote` is needed for rewards.
    pub fn try_new(
        cluster_info: &ClusterInfo,
        leader_schedule: &LeaderScheduleCache,
        root_slot: Slot,
        msg: &VoteMessage,
        stake: NonZero<u64>,
        vote_account_pubkey: Pubkey,
    ) -> Option<Self> {
        let vote_slot = msg.vote.slot();
        let vote = match &msg.vote {
            Vote::Notarize(notar) => RewardVote::Notar(*notar),
            Vote::Skip(skip) => RewardVote::Skip(*skip),
            Vote::Finalize(_)
            | Vote::NotarizeFallback(_)
            | Vote::SkipFallback(_)
            | Vote::Genesis(_) => return None,
        };
        if !is_relevant(vote_slot, root_slot, cluster_info, leader_schedule) {
            return None;
        }
        Some(Self {
            vote,
            signature: msg.signature,
            rank: msg.rank,
            stake,
            vote_account_pubkey,
        })
    }
}

impl From<RewardVoteMessage> for VoteMessage {
    fn from(msg: RewardVoteMessage) -> Self {
        let vote = match msg.vote {
            RewardVote::Notar(notar) => Vote::new_notarization_vote(notar.block),
            RewardVote::Skip(skip) => Vote::new_skip_vote(skip.slot),
        };
        Self {
            vote,
            signature: msg.signature,
            rank: msg.rank,
        }
    }
}

#[must_use]
/// Returns true if the given `msg` is needed for rewards.
pub(crate) fn rewards_wants_unverified_vote_msg(
    cluster_info: &ClusterInfo,
    leader_schedule: &LeaderScheduleCache,
    root_slot: Slot,
    msg: &UnverifiedVoteMessage,
) -> bool {
    match &msg.vote {
        Vote::Finalize(_)
        | Vote::NotarizeFallback(_)
        | Vote::SkipFallback(_)
        | Vote::Genesis(_) => return false,
        Vote::Notarize(_) | Vote::Skip(_) => (),
    }
    let vote_slot = msg.vote.slot();
    is_relevant(vote_slot, root_slot, cluster_info, leader_schedule)
}

#[must_use]
/// Returns true if a reward vote at the `vote_slot` is needed by this node for rewards.
fn is_relevant(
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
