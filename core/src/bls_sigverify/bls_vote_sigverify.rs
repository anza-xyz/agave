#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    super::{errors::SigVerifyVoteError, stats::SigVerifyVoteStats},
    crate::{
        bls_sigverify::{
            bls_sigverifier::NUM_SLOTS_FOR_VERIFY,
            utils::{
                send_votes_to_metrics, send_votes_to_pool, send_votes_to_repair,
                send_votes_to_rewards,
            },
        },
        cluster_info_vote_listener::VerifiedVoterSlotsSender,
    },
    agave_votor::{
        consensus_metrics::{ConsensusMetricsEvent, ConsensusMetricsEventSender},
        consensus_rewards,
    },
    agave_votor_messages::{
        consensus_message::{ConsensusMessage, VoteMessage},
        reward_certificate::AddVoteMessage,
        vote::Vote,
    },
    crossbeam_channel::Sender,
    rayon::{
        ThreadPool, current_thread_index,
        iter::{IntoParallelIterator, IntoParallelRefIterator, ParallelIterator},
    },
    solana_bls_signatures::{
        BlsError,
        pubkey::{Pubkey as BlsPubkey, PubkeyProjective, VerifiablePubkey},
        signature::SignatureProjective,
    },
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_measure::{measure::Measure, measure_us},
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    std::collections::HashMap,
};

/// [`VoteMessage`] along with other information needed to sig verify it.
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
#[derive(Clone, Debug)]
pub(super) struct VotePayload {
    pub vote_message: VoteMessage,
    pub bls_pubkey: BlsPubkey,
    pub pubkey: Pubkey,
}

impl VotePayload {
    fn verify(self) -> Option<Self> {
        let Ok(payload) = wincode::serialize(&self.vote_message.vote) else {
            return None;
        };
        self.bls_pubkey
            .verify_signature(&self.vote_message.signature, &payload)
            .is_ok()
            .then_some(self)
    }
}

/// Verifies votes and sends the verified votes to the consensus pool; and sends the desired subset
/// to rewards container and repair.
///
/// Returns the Vec of [`VoteToVerify`] to the caller to enable reuse.  The length of the returned
/// buffer might be lower than the input buffer.
#[allow(clippy::too_many_arguments)]
pub(super) fn verify_and_send_votes(
    votes_to_verify: Vec<VotePayload>,
    root_bank: &Bank,
    cluster_info: &ClusterInfo,
    leader_schedule: &LeaderScheduleCache,
    channel_to_pool: &Sender<Vec<ConsensusMessage>>,
    channel_to_repair: &VerifiedVoterSlotsSender,
    channel_to_reward: &Sender<AddVoteMessage>,
    channel_to_metrics: &ConsensusMetricsEventSender,
    thread_pool: &ThreadPool,
) -> Result<SigVerifyVoteStats, SigVerifyVoteError> {
    let mut measure = Measure::start("verify_and_send_votes");
    let mut stats = SigVerifyVoteStats::default();
    if votes_to_verify.is_empty() {
        return Ok(stats);
    }
    stats.votes_to_sig_verify += votes_to_verify.len() as u64;
    let verified_votes = verify_votes(root_bank, votes_to_verify, &mut stats, thread_pool);
    stats.sig_verified_votes += verified_votes.len() as u64;

    let (votes_for_pool, msgs_for_repair, msg_for_reward, msg_for_metrics) =
        process_verified_votes(&verified_votes, root_bank, cluster_info, leader_schedule);

    send_votes_to_pool(votes_for_pool, channel_to_pool, &mut stats)?;
    send_votes_to_repair(msgs_for_repair, channel_to_repair, &mut stats)?;
    send_votes_to_rewards(msg_for_reward, channel_to_reward, &mut stats)?;
    send_votes_to_metrics(msg_for_metrics, channel_to_metrics, &mut stats)?;

    measure.stop();
    stats
        .fn_verify_and_send_votes_stats
        .add_sample(measure.as_us());
    Ok(stats)
}

/// If the vote is relevant to repair, then adds it to the [`msgs_for_repair`] so it can eventually
/// be sent to repair.
fn inspect_for_repair(vote: &VotePayload, msgs_for_repair: &mut HashMap<Pubkey, Vec<Slot>>) {
    let vote_slot = vote.vote_message.vote.slot();
    match vote.vote_message.vote {
        Vote::Notarize(_) | Vote::Finalize(_) | Vote::NotarizeFallback(_) => {
            msgs_for_repair
                .entry(vote.pubkey)
                .or_default()
                .push(vote_slot);
        }
        Vote::Skip(_) | Vote::SkipFallback(_) | Vote::Genesis(_) => (),
    }
}

/// Processes the verified votes for various downstream services.
///
/// In particular, collects and returns the relevant messages for the consensus pool; rewards;
/// repair; and metrics;
fn process_verified_votes(
    verified_votes: &[VotePayload],
    root_bank: &Bank,
    cluster_info: &ClusterInfo,
    leader_schedule: &LeaderScheduleCache,
) -> (
    Vec<ConsensusMessage>,
    HashMap<Pubkey, Vec<Slot>>,
    AddVoteMessage,
    Vec<ConsensusMetricsEvent>,
) {
    let mut votes_for_reward = Vec::with_capacity(verified_votes.len());
    let mut msgs_for_repair = HashMap::new();
    let mut votes_for_pool = Vec::with_capacity(verified_votes.len());
    let mut votes_for_metrics = Vec::with_capacity(verified_votes.len());
    for vote in verified_votes {
        let vote_message = vote.vote_message;
        if consensus_rewards::wants_vote(
            cluster_info,
            leader_schedule,
            root_bank.slot(),
            &vote_message,
        ) {
            votes_for_reward.push(vote_message);
        }

        inspect_for_repair(vote, &mut msgs_for_repair);

        votes_for_pool.push(ConsensusMessage::Vote(vote_message));

        votes_for_metrics.push(ConsensusMetricsEvent::Vote {
            id: vote.pubkey,
            vote: vote.vote_message.vote,
        });
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
        votes_for_pool,
        msgs_for_repair,
        AddVoteMessage {
            votes: votes_for_reward,
        },
        votes_for_metrics,
    )
}

/// Sig verifies `votes_to_verify` and returns a `Vec` of votes that passed verification.
fn verify_votes(
    root_bank: &Bank,
    votes_to_verify: Vec<VotePayload>,
    stats: &mut SigVerifyVoteStats,
    thread_pool: &ThreadPool,
) -> Vec<VotePayload> {
    // Filter votes too far in the future.
    let len_before = votes_to_verify.len();
    let votes_to_verify = votes_to_verify
        .into_iter()
        .filter(|v| {
            v.vote_message.vote.slot() <= root_bank.slot().saturating_add(NUM_SLOTS_FOR_VERIFY)
        })
        .collect::<Vec<_>>();
    let num_discarded = len_before - votes_to_verify.len();
    stats.too_far_in_future = stats.too_far_in_future.saturating_add(num_discarded as u64);

    // Try optimistic verification - fast to verify, but cannot identify invalid votes
    if verify_votes_optimistic(&votes_to_verify, stats, thread_pool) {
        return votes_to_verify;
    }

    // Fallback to individual verification
    let (verified_votes, time_us) =
        measure_us!(verify_individual_votes(votes_to_verify, thread_pool));
    stats.fn_verify_individual_votes_stats.add_sample(time_us);
    verified_votes
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
fn verify_votes_optimistic(
    votes: &[VotePayload],
    stats: &mut SigVerifyVoteStats,
    thread_pool: &ThreadPool,
) -> bool {
    let mut measure = Measure::start("verify_votes_optimistic");

    // For BLS verification, minimizing the expensive pairing operation is key.
    // Each BLS signature verification requires two pairings.
    //
    // However, the BLS verification formula allows us to:
    // 1. Aggregate all signatures into a single signature.
    // 2. Aggregate public keys for each unique message.
    //
    // By verifying the aggregated signature against the aggregated public keys,
    // the number of pairings required is reduced to (1 + number of distinct messages).
    let (signature_result, (distinct_payloads, pubkeys_result)) = thread_pool.join(
        || aggregate_signatures(votes),
        || aggregate_pubkeys_by_payload(votes, stats),
    );

    let Ok(aggregate_signature) = signature_result else {
        return false;
    };

    let Ok(aggregate_pubkeys) = pubkeys_result else {
        return false;
    };

    let verified = if distinct_payloads.len() == 1 {
        // if one unique payload, just verify the aggregate signature for the single payload
        // this requires (2 pairings)
        aggregate_pubkeys[0]
            .verify_signature(&aggregate_signature, &distinct_payloads[0])
            .is_ok()
    } else {
        // if non-unique payload, we need to apply a pairing for each distinct message,
        // which is done inside `par_verify_distinct_aggregated`.
        let payload_slices: Vec<&[u8]> = distinct_payloads.iter().map(|p| p.as_slice()).collect();
        thread_pool.install(|| {
            SignatureProjective::par_verify_distinct_aggregated(
                &aggregate_pubkeys,
                &aggregate_signature,
                &payload_slices,
            )
            .is_ok()
        })
    };

    measure.stop();
    stats
        .fn_verify_votes_optimistic_stats
        .add_sample(measure.as_us());
    verified
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
fn aggregate_signatures(votes: &[VotePayload]) -> Result<SignatureProjective, BlsError> {
    debug_assert!(current_thread_index().is_some());
    let signatures = votes.par_iter().map(|v| &v.vote_message.signature);
    // TODO(sam): Currently, `par_aggregate` performs full validation
    // (on-curve + subgroup check) for every signature. Since the subgroup
    // check is expensive, we can use an `unchecked` deserialization here
    // (performing only the cheap on-curve check) and rely on a single subgroup
    // check on the final aggregated signature. This should save more than 80%
    // of the time for signature aggregation.
    SignatureProjective::par_aggregate(signatures)
}

#[allow(clippy::type_complexity)]
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
fn aggregate_pubkeys_by_payload(
    votes: &[VotePayload],
    stats: &mut SigVerifyVoteStats,
) -> (Vec<Vec<u8>>, Result<Vec<PubkeyProjective>, BlsError>) {
    debug_assert!(current_thread_index().is_some());
    let mut grouped_votes: HashMap<&Vote, Vec<&BlsPubkey>> = HashMap::new();

    for v in votes {
        grouped_votes
            .entry(&v.vote_message.vote)
            .or_default()
            .push(&v.bls_pubkey);
    }

    stats
        .distinct_votes_stats
        .add_sample(grouped_votes.len() as u64);

    let (distinct_payloads, distinct_pubkeys_results): (Vec<_>, Vec<_>) = grouped_votes
        .into_par_iter()
        .filter_map(|(vote, pubkeys)| {
            wincode::serialize(vote).ok().map(|vote| {
                // TODO(sam): https://github.com/anza-xyz/alpenglow/issues/708
                // should improve public key aggregation drastically (more than 80%)
                let pubkey = PubkeyProjective::par_aggregate(pubkeys.into_par_iter());
                (vote, pubkey)
            })
        })
        .unzip();
    let aggregate_pubkeys_result = distinct_pubkeys_results.into_iter().collect();

    (distinct_payloads, aggregate_pubkeys_result)
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
fn verify_individual_votes(
    votes_to_verify: Vec<VotePayload>,
    thread_pool: &ThreadPool,
) -> Vec<VotePayload> {
    thread_pool.install(|| {
        votes_to_verify
            .into_par_iter()
            .filter_map(|vote| vote.verify())
            .collect()
    })
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    #[should_panic]
    fn ensure_aggregate_signatures_runs_on_thread_pool() {
        let votes = vec![];
        // calling without a rayon thread pool should trigger a debug assert.
        aggregate_signatures(&votes).unwrap();
    }

    #[test]
    #[should_panic]
    fn ensure_aggregate_pubkeys_by_payload_runs_on_thread_pool() {
        let votes = vec![];
        let mut stats = SigVerifyVoteStats::default();
        // calling without a rayon thread pool should trigger a debug assert.
        aggregate_pubkeys_by_payload(&votes, &mut stats).1.unwrap();
    }
}
