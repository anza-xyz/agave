use {
    crate::{
        bls_sigverifier::SigVerifierChannels,
        errors::SigVerifyVoteError,
        rewards::{AddVoteMessage, rewards_wants_vote},
        utils::{send_votes_to_metrics, send_votes_to_repair, send_votes_to_rewards},
        vote_pool::{VerifiedVotePayload, VotePool},
    },
    agave_votor_messages::{metric_types::ConsensusMetricsEvent, vote::Vote},
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    std::collections::HashMap,
};

impl VotePool {
    pub(super) fn process_verified_votes(
        &mut self,
        root_bank: &Bank,
        verified_votes: Vec<VerifiedVotePayload>,
        channels: &SigVerifierChannels,
    ) -> Result<(), SigVerifyVoteError> {
        let (msgs_for_repair, msg_for_reward, msg_for_metrics) = process_verified_votes(
            verified_votes,
            root_bank,
            &self.cluster_info,
            &self.leader_schedule,
        );

        send_votes_to_repair(
            msgs_for_repair,
            &channels.channel_to_repair,
            &mut self.stats,
        )?;
        send_votes_to_rewards(msg_for_reward, &channels.channel_to_reward, &mut self.stats)?;
        send_votes_to_metrics(
            msg_for_metrics,
            &channels.channel_to_metrics,
            &mut self.stats,
        )?;
        Ok(())
    }
}

/// Processes the verified votes for various downstream services.
///
/// In particular, collects and returns the relevant messages for the consensus pool; rewards;
/// repair; and metrics;
fn process_verified_votes(
    verified_votes: Vec<VerifiedVotePayload>,
    root_bank: &Bank,
    cluster_info: &ClusterInfo,
    leader_schedule: &LeaderScheduleCache,
) -> (
    HashMap<Pubkey, Vec<Slot>>,
    AddVoteMessage,
    Vec<ConsensusMetricsEvent>,
) {
    let mut votes_for_reward = Vec::with_capacity(verified_votes.len());
    let mut msgs_for_repair = HashMap::new();
    let mut vote_aggregates_for_pool = Vec::with_capacity(verified_votes.len());
    let mut votes_for_metrics = Vec::with_capacity(verified_votes.len());
    for payload in verified_votes {
        if rewards_wants_vote(
            cluster_info,
            leader_schedule,
            root_bank.slot(),
            &payload.vote_aggregate.vote,
        ) {
            votes_for_reward.push(payload.vote_aggregate.clone());
        }

        inspect_for_repair(&payload, &mut msgs_for_repair);

        for pubkey in &payload.sender_vote_account_pubkeys {
            votes_for_metrics.push(ConsensusMetricsEvent::Vote {
                id: *pubkey,
                vote: payload.vote_aggregate.vote,
            });
        }
        vote_aggregates_for_pool.push(payload.vote_aggregate);
    }
    let msgs_for_repair = msgs_for_repair
        .into_iter()
        .map(|(pubkey, mut slots)| {
            slots.sort_unstable();
            slots.dedup();
            (pubkey, slots)
        })
        .collect();
    (
        msgs_for_repair,
        AddVoteMessage {
            votes: votes_for_reward,
        },
        votes_for_metrics,
    )
}

/// If the vote is relevant to repair, then adds it to the [`msgs_for_repair`] so it can eventually
/// be sent to repair.
fn inspect_for_repair(
    vote: &VerifiedVotePayload,
    msgs_for_repair: &mut HashMap<Pubkey, Vec<Slot>>,
) {
    let vote_slot = vote.vote_aggregate.vote.slot();
    match vote.vote_aggregate.vote {
        Vote::Notarize(_) | Vote::Finalize(_) | Vote::NotarizeFallback(_) => {
            for pubkey in &vote.sender_vote_account_pubkeys {
                msgs_for_repair.entry(*pubkey).or_default().push(vote_slot);
            }
        }
        Vote::Skip(_) | Vote::SkipFallback(_) | Vote::Genesis(_) => (),
    }
}
