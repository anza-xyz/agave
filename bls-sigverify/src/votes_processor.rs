use {
    crate::{
        bls_vote_sigverify::{ProcessedVotes, VerifiedVotePayload},
        errors::SigVerifyVoteError,
        rewards::{RewardInput, rewards_wants_vote},
        stats::VoteSenderStats,
        utils::{
            send_sig_verified_batch_to_pool, send_votes_to_metrics, send_votes_to_repair,
            send_votes_to_rewards,
        },
    },
    agave_votor_messages::{
        VerifiedVoterSlotsSender,
        metric_types::{ConsensusMetricsEvent, ConsensusMetricsEventSender},
        sig_verified_messages::SigVerifiedBatch,
        vote::Vote,
    },
    crossbeam_channel::{Receiver, Sender},
    log::error,
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::SharableBanks},
    std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    },
};

pub(crate) struct VotesProcessor {
    exit: Arc<AtomicBool>,
    sharable_banks: SharableBanks,
    cluster_info: Arc<ClusterInfo>,
    leader_schedule: Arc<LeaderScheduleCache>,
    channel_to_repair: VerifiedVoterSlotsSender,
    channel_to_reward: Sender<RewardInput>,
    channel_to_pool: Sender<SigVerifiedBatch>,
    channel_to_metrics: ConsensusMetricsEventSender,
    verified_votes_receiver: Receiver<Vec<Vec<VerifiedVotePayload>>>,
}

impl VotesProcessor {
    pub(crate) fn new(
        exit: Arc<AtomicBool>,
        verified_votes_receiver: Receiver<Vec<Vec<VerifiedVotePayload>>>,
        sharable_banks: SharableBanks,
        cluster_info: Arc<ClusterInfo>,
        leader_schedule: Arc<LeaderScheduleCache>,
        channel_to_repair: VerifiedVoterSlotsSender,
        channel_to_reward: Sender<RewardInput>,
        channel_to_pool: Sender<SigVerifiedBatch>,
        channel_to_metrics: ConsensusMetricsEventSender,
    ) -> Self {
        Self {
            exit,
            verified_votes_receiver,
            sharable_banks,
            cluster_info,
            leader_schedule,
            channel_to_repair,
            channel_to_reward,
            channel_to_pool,
            channel_to_metrics,
        }
    }

    fn recv(&self) -> Result<Vec<Vec<VerifiedVotePayload>>, ()> {
        self.verified_votes_receiver.recv().map_err(|_| ())
    }

    fn process_votes(&self, verified_votes: Vec<Vec<VerifiedVotePayload>>) -> Vec<ProcessedVotes> {
        let root_bank = self.sharable_banks.root();
        process_verified_votes(
            verified_votes,
            &root_bank,
            &self.cluster_info,
            &self.leader_schedule,
        )
    }

    fn send_msgs(
        &self,
        processed_votess: Vec<ProcessedVotes>,
    ) -> Result<VoteSenderStats, SigVerifyVoteError> {
        let my_pubkey = &self.cluster_info.id();
        let mut sender_stats = VoteSenderStats::default();
        for processed_votes in processed_votess {
            send_sig_verified_batch_to_pool(
                my_pubkey,
                SigVerifiedBatch::Votes(processed_votes.vote_aggregates_for_pool),
                &self.channel_to_pool,
                &mut sender_stats,
            )?;
            send_votes_to_repair(
                my_pubkey,
                processed_votes.repair_msg,
                &self.channel_to_repair,
                &mut sender_stats,
            );
            send_votes_to_rewards(
                my_pubkey,
                processed_votes.reward_msg,
                &self.channel_to_reward,
                &mut sender_stats,
            );
            send_votes_to_metrics(
                my_pubkey,
                processed_votes.metrics_msg,
                &self.channel_to_metrics,
                &mut sender_stats,
            );
        }
        Ok(sender_stats)
    }

    pub(crate) fn run(self) {
        while !self.exit.load(Ordering::Relaxed) {
            let Ok(votes) = self.recv() else {
                error!("verified votes receiver channel disconnected.  Exiting.");
                break;
            };
            let processed_votes = self.process_votes(votes);
            if let Err(e) = self.send_msgs(processed_votes) {
                error!("sending msgs failed with {e:?}.  Exiting.");
                break;
            }
        }
    }
}

/// Processes the verified votes for various downstream services.
///
/// In particular, collects and returns the relevant messages for the consensus pool; rewards;
/// repair; and metrics;
fn process_verified_votes(
    verified_votess: Vec<Vec<VerifiedVotePayload>>,
    root_bank: &Bank,
    cluster_info: &ClusterInfo,
    leader_schedule: &LeaderScheduleCache,
) -> Vec<ProcessedVotes> {
    let mut ret = vec![];
    for verified_votes in verified_votess {
        let mut votes_for_reward = Vec::with_capacity(verified_votes.len());
        let mut msgs_for_repair = HashMap::new();
        let mut vote_aggregates_for_pool = Vec::with_capacity(verified_votes.len());
        let mut votes_for_metrics = Vec::with_capacity(verified_votes.len());
        for payload in verified_votes {
            inspect_for_repair(&payload, &mut msgs_for_repair);

            for pubkey in &payload.sender_vote_account_pubkeys {
                votes_for_metrics.push(ConsensusMetricsEvent::Vote {
                    id: *pubkey,
                    vote: *payload.vote_aggregate.vote(),
                });
            }
            if rewards_wants_vote(
                cluster_info,
                leader_schedule,
                root_bank.slot(),
                payload.vote_aggregate.vote(),
            ) {
                votes_for_reward.push(payload.vote_aggregate.clone());
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
        let p = ProcessedVotes {
            reward_msg: votes_for_reward,
            repair_msg: msgs_for_repair,
            vote_aggregates_for_pool,
            metrics_msg: votes_for_metrics,
        };
        ret.push(p);
    }
    ret
}

/// If the vote is relevant to repair, then adds it to the [`msgs_for_repair`] so it can eventually
/// be sent to repair.
fn inspect_for_repair(
    vote: &VerifiedVotePayload,
    msgs_for_repair: &mut HashMap<Pubkey, Vec<Slot>>,
) {
    let vote_slot = vote.vote_aggregate.vote().slot();
    match vote.vote_aggregate.vote() {
        Vote::Notarize(_) | Vote::Finalize(_) | Vote::NotarizeFallback(_) => {
            for pubkey in &vote.sender_vote_account_pubkeys {
                msgs_for_repair.entry(*pubkey).or_default().push(vote_slot);
            }
        }
        Vote::Skip(_) | Vote::SkipFallback(_) | Vote::Genesis(_) => (),
    }
}
