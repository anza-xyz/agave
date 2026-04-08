#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    super::{errors::SigVerifyVoteError, stats::SigVerifyVoteStats},
    crate::bls_sigverify::{
        bls_sigverifier::{BAN_TIMEOUT, NUM_SLOTS_FOR_VERIFY, SigVerifierChannels},
        utils::{
            send_votes_to_metrics, send_votes_to_pool, send_votes_to_repair, send_votes_to_rewards,
        },
    },
    agave_votor::{consensus_metrics::ConsensusMetricsEvent, consensus_rewards},
    agave_votor_messages::{
        consensus_message::{ConsensusMessage, VoteMessage},
        reward_certificate::AddVoteMessage,
        vote::Vote,
    },
    rayon::{
        ThreadPool, current_thread_index,
        iter::{Either, IntoParallelIterator, IntoParallelRefIterator, ParallelIterator},
    },
    solana_bls_signatures::{
        BlsError, PreparedHashedMessage,
        pubkey::{PubkeyAffine as BlsPubkeyAffine, PubkeyProjective, VerifiablePubkey},
        signature::SignatureProjective,
    },
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_measure::{measure::Measure, measure_us},
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_streamer::nonblocking::simple_qos::SimpleQosBanlist,
    std::{collections::HashMap, sync::Arc},
};

/// This is the percentage threshold of distinct votes among the total votes under which we will prepare a cache of
/// prepared payloads for individual verification.
const PREPARED_PAYLOAD_CACHE_DISTINCT_VOTE_THRESHOLD_PERCENT: usize = 90;

/// [`VoteMessage`] along with other information needed to sig verify it.
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
#[derive(Clone, Debug)]
pub(super) struct VotePayload {
    pub vote_message: VoteMessage,
    pub bls_pubkey: BlsPubkeyAffine,
    pub pubkey: Pubkey,
    pub remote_pubkey: Pubkey,
    pub prepared_payload: Option<Arc<PreparedHashedMessage>>,
}

impl VotePayload {
    fn verify(self) -> Option<Self> {
        let is_verified = if let Some(prepared_payload) = self.prepared_payload.as_deref() {
            self.bls_pubkey
                .verify_signature_prepared(&self.vote_message.signature, prepared_payload)
                .is_ok()
        } else {
            let payload = wincode::serialize(&self.vote_message.vote).ok()?;
            self.bls_pubkey
                .verify_signature(&self.vote_message.signature, &payload)
                .is_ok()
        };
        is_verified.then_some(self)
    }
}

/// Verifies votes and sends the verified votes to the consensus pool; and sends the desired subset
/// to rewards container and repair.
///
/// Any vote that fails fallback individual signature verification will have its sender banlisted.
pub(super) fn verify_and_send_votes(
    votes_to_verify: Vec<VotePayload>,
    root_bank: &Bank,
    cluster_info: &ClusterInfo,
    leader_schedule: &LeaderScheduleCache,
    banlist: &SimpleQosBanlist,
    thread_pool: &ThreadPool,
    channels: &SigVerifierChannels,
) -> Result<SigVerifyVoteStats, SigVerifyVoteError> {
    let mut measure = Measure::start("verify_and_send_votes");
    let mut stats = SigVerifyVoteStats::default();
    if votes_to_verify.is_empty() {
        return Ok(stats);
    }
    stats.votes_to_sig_verify += votes_to_verify.len() as u64;
    let verified_votes = verify_votes(root_bank, votes_to_verify, &mut stats, banlist, thread_pool);
    stats.sig_verified_votes += verified_votes.len() as u64;

    let (votes_for_pool, msgs_for_repair, msg_for_reward, msg_for_metrics) =
        process_verified_votes(&verified_votes, root_bank, cluster_info, leader_schedule);

    send_votes_to_pool(votes_for_pool, &channels.channel_to_pool, &mut stats)?;
    send_votes_to_repair(msgs_for_repair, &channels.channel_to_repair, &mut stats)?;
    send_votes_to_rewards(msg_for_reward, &channels.channel_to_reward, &mut stats)?;
    send_votes_to_metrics(msg_for_metrics, &channels.channel_to_metrics, &mut stats)?;

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
    banlist: &SimpleQosBanlist,
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
    let (optimistic_result, distinct_votes, distinct_payloads) =
        verify_votes_optimistic(&votes_to_verify, stats, thread_pool);
    if matches!(optimistic_result, OptimisticVerificationResult::Verified) {
        return votes_to_verify;
    }

    // Fallback to individual verification
    let ((verified_votes, invalid_remote_pubkeys), time_us) = measure_us!(verify_individual_votes(
        votes_to_verify,
        distinct_votes,
        distinct_payloads,
        thread_pool,
    ));
    for remote_pubkey in invalid_remote_pubkeys {
        if banlist.ban(remote_pubkey, BAN_TIMEOUT) {
            stats.already_banned += 1;
        };
    }
    stats.fn_verify_individual_votes_stats.add_sample(time_us);

    verified_votes
}

/// Outcome of optimistic aggregate vote verification.
///
/// `Failed` carries the number of distinct vote messages seen before falling
/// back to individual verification.
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OptimisticVerificationResult {
    Verified,
    Failed { num_distinct_messages: usize },
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
/// Attempts aggregate BLS verification across the full vote set.
///
/// This fast path aggregates all vote signatures and the public keys for each
/// distinct vote payload, minimizing the number of pairing operations needed
/// for verification. When aggregation or aggregate verification fails, the
/// caller falls back to individual vote verification so invalid votes can be
/// identified precisely.
///
/// Returns the optimistic verification outcome together with the distinct vote
/// messages and their prepared payloads, which can be reused by the fallback
/// path.
fn verify_votes_optimistic(
    votes: &[VotePayload],
    stats: &mut SigVerifyVoteStats,
    thread_pool: &ThreadPool,
) -> (
    OptimisticVerificationResult,
    Vec<Vote>,
    Vec<PreparedHashedMessage>,
) {
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
    let (signature_result, (distinct_votes, distinct_payloads, pubkeys_result)) = thread_pool.join(
        || aggregate_signatures(votes),
        || aggregate_pubkeys_by_payload(votes, stats),
    );

    let Ok(aggregate_signature) = signature_result else {
        return (
            OptimisticVerificationResult::Failed {
                num_distinct_messages: 0,
            },
            Vec::new(),
            Vec::new(),
        );
    };

    let Ok(aggregate_pubkeys) = pubkeys_result else {
        return (
            OptimisticVerificationResult::Failed {
                num_distinct_messages: distinct_payloads.len(),
            },
            distinct_votes,
            distinct_payloads,
        );
    };

    let verified = if distinct_payloads.len() == 1 {
        // if one unique payload, just verify the aggregate signature for the single payload
        // this requires (2 pairings)
        aggregate_pubkeys[0]
            .verify_signature_prepared(&aggregate_signature, &distinct_payloads[0])
            .is_ok()
    } else {
        // if non-unique payload, we need to apply a pairing for each distinct message,
        // which is done inside `par_verify_distinct_aggregated_prepared`.
        thread_pool.install(|| {
            SignatureProjective::par_verify_distinct_aggregated_prepared(
                &aggregate_pubkeys,
                &aggregate_signature,
                &distinct_payloads,
            )
            .is_ok()
        })
    };

    measure.stop();
    stats
        .fn_verify_votes_optimistic_stats
        .add_sample(measure.as_us());
    let result = if verified {
        OptimisticVerificationResult::Verified
    } else {
        OptimisticVerificationResult::Failed {
            num_distinct_messages: distinct_payloads.len(),
        }
    };
    (result, distinct_votes, distinct_payloads)
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
) -> (
    Vec<Vote>,
    Vec<PreparedHashedMessage>,
    Result<Vec<PubkeyProjective>, BlsError>,
) {
    debug_assert!(current_thread_index().is_some());
    let mut grouped_votes: HashMap<&Vote, Vec<&BlsPubkeyAffine>> = HashMap::new();

    for v in votes {
        grouped_votes
            .entry(&v.vote_message.vote)
            .or_default()
            .push(&v.bls_pubkey);
    }

    stats
        .distinct_votes_stats
        .add_sample(grouped_votes.len() as u64);

    let distinct_grouped_votes = grouped_votes
        .into_par_iter()
        .filter_map(|(vote, pubkeys)| {
            wincode::serialize(vote).ok().map(|serialized_vote| {
                // TODO(sam): https://github.com/anza-xyz/alpenglow/issues/708
                // should improve public key aggregation drastically (more than 80%)
                let pubkey = PubkeyProjective::par_aggregate(pubkeys.into_par_iter());
                (*vote, PreparedHashedMessage::new(&serialized_vote), pubkey)
            })
        })
        .collect::<Vec<_>>();
    let (distinct_votes, distinct_payloads, distinct_pubkeys_results): (Vec<_>, Vec<_>, Vec<_>) =
        distinct_grouped_votes.into_iter().fold(
            (Vec::new(), Vec::new(), Vec::new()),
            |mut acc, (vote, payload, pubkey)| {
                acc.0.push(vote);
                acc.1.push(payload);
                acc.2.push(pubkey);
                acc
            },
        );
    let aggregate_pubkeys_result = distinct_pubkeys_results.into_iter().collect();

    (distinct_votes, distinct_payloads, aggregate_pubkeys_result)
}

/// Verifies votes individually on a thread pool.
///
/// Returns:
/// - `Vec<VotePayload>`: votes that passed verification.
/// - `Vec<Pubkey>`: remote pubkeys for votes that failed verification.
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
fn verify_individual_votes(
    mut votes_to_verify: Vec<VotePayload>,
    distinct_votes: Vec<Vote>,
    distinct_payloads: Vec<PreparedHashedMessage>,
    thread_pool: &ThreadPool,
) -> (Vec<VotePayload>, Vec<Pubkey>) {
    if should_prepare_payload_cache(distinct_votes.len(), votes_to_verify.len()) {
        let prepared_payloads: HashMap<_, _> = distinct_votes
            .into_iter()
            .zip(distinct_payloads)
            .map(|(vote, payload)| (vote, Arc::new(payload)))
            .collect();
        for vote in &mut votes_to_verify {
            vote.prepared_payload = prepared_payloads.get(&vote.vote_message.vote).cloned();
        }
    }

    thread_pool.install(|| {
        votes_to_verify.into_par_iter().partition_map(|vote| {
            let remote_pubkey = vote.remote_pubkey;
            match vote.verify() {
                Some(vote) => Either::Left(vote),
                None => Either::Right(remote_pubkey),
            }
        })
    })
}

fn should_prepare_payload_cache(distinct_vote_count: usize, total_vote_count: usize) -> bool {
    distinct_vote_count.saturating_mul(100)
        <= total_vote_count.saturating_mul(PREPARED_PAYLOAD_CACHE_DISTINCT_VOTE_THRESHOLD_PERCENT)
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn test_should_prepare_payload_cache() {
        assert!(should_prepare_payload_cache(9, 10));
        assert!(should_prepare_payload_cache(0, 0));
        assert!(!should_prepare_payload_cache(1, 1));
        assert!(!should_prepare_payload_cache(10, 10));
        assert!(!should_prepare_payload_cache(91, 100));
        assert!(should_prepare_payload_cache(90, 100));
    }

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
        aggregate_pubkeys_by_payload(&votes, &mut stats).2.unwrap();
    }
}
