#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    super::{errors::SigVerifyVoteError, stats::SigVerifyVoteStats},
    crate::{
        bls_sigverify::utils::{
            send_votes_to_metrics, send_votes_to_pool, send_votes_to_repair, send_votes_to_rewards,
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
    rayon::iter::{IntoParallelIterator, IntoParallelRefIterator, ParallelIterator},
    solana_bls_signatures::{
        BlsError,
        pubkey::{Pubkey as BlsPubkey, PubkeyProjective, VerifiablePubkey},
        signature::SignatureProjective,
    },
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_measure::measure::Measure,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    std::collections::{HashMap, HashSet},
};

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
#[derive(Clone, Debug)]
pub(super) struct VoteToVerify {
    pub vote_message: VoteMessage,
    pub bls_pubkey: BlsPubkey,
    pub pubkey: Pubkey,
}

impl VoteToVerify {
    fn verify(&self) -> bool {
        let Ok(payload) = bincode::serialize(&self.vote_message.vote) else {
            return false;
        };
        self.bls_pubkey
            .verify_signature(&self.vote_message.signature, &payload)
            .is_ok()
    }
}

/// Verifies votes and sends the verified votes to the consensus pool; and sends the desired subset
/// to rewards container and repair.
///
/// Returns the Vec of [`VoteToVerify`] to the caller to enable reuse.  The length of the returned
/// buffer might be lower than the input buffer.
#[allow(clippy::too_many_arguments)]
pub(super) fn verify_and_send_votes(
    votes_to_verify: Vec<VoteToVerify>,
    root_bank: &Bank,
    cluster_info: &ClusterInfo,
    leader_schedule: &LeaderScheduleCache,
    channel_to_pool: &Sender<Vec<ConsensusMessage>>,
    channel_to_repair: &VerifiedVoterSlotsSender,
    channel_to_reward: &Sender<AddVoteMessage>,
    channel_to_metrics: &ConsensusMetricsEventSender,
) -> Result<(Vec<VoteToVerify>, SigVerifyVoteStats), SigVerifyVoteError> {
    let mut measure = Measure::start("verify_and_send_votes");
    let mut stats = SigVerifyVoteStats::default();
    if votes_to_verify.is_empty() {
        return Ok((votes_to_verify, stats));
    }
    stats.votes_to_sig_verify += votes_to_verify.len() as u64;
    let verified_votes = verify_votes(votes_to_verify, &mut stats);
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
        .increment(measure.as_us())
        .unwrap();
    Ok((verified_votes, stats))
}

/// If the vote is relevant to repair, then adds it to the [`msgs_for_repair`] so it can eventually
/// be sent to repair.
fn inspect_for_repair(vote: &VoteToVerify, msgs_for_repair: &mut HashMap<Pubkey, HashSet<Slot>>) {
    let vote_slot = vote.vote_message.vote.slot();
    match vote.vote_message.vote {
        Vote::Notarize(_) | Vote::Finalize(_) | Vote::NotarizeFallback(_) => {
            msgs_for_repair
                .entry(vote.pubkey)
                .or_default()
                .insert(vote_slot);
        }
        Vote::Skip(_) | Vote::SkipFallback(_) | Vote::Genesis(_) => (),
    }
}

/// Processes the verified votes for various downstream services.
///
/// In particular, collects and returns the relevant messages for the consensus pool; rewards;
/// repair; and metrics;
fn process_verified_votes(
    verified_votes: &[VoteToVerify],
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
        .map(|(k, v)| (k, v.into_iter().collect()))
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
fn verify_votes(
    votes_to_verify: Vec<VoteToVerify>,
    stats: &mut SigVerifyVoteStats,
) -> Vec<VoteToVerify> {
    // Try optimistic verification - fast to verify, but cannot identify invalid votes
    if verify_votes_optimistic(&votes_to_verify, stats) {
        return votes_to_verify;
    }

    // Fallback to individual verification
    verify_individual_votes(votes_to_verify, stats)
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
fn verify_votes_optimistic(
    votes_to_verify: &[VoteToVerify],
    stats: &mut SigVerifyVoteStats,
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
    //
    // Assuming that sigverifier's dedicated thread pool was used to call this function, the
    // following should run on that thread pool.
    let (signature_result, (distinct_payloads, pubkeys_result)) = rayon::join(
        || aggregate_signatures(votes_to_verify),
        || aggregate_pubkeys_by_payload(votes_to_verify, stats),
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
        //
        // Assuming that sigverifier's dedicated thread pool was used to call this function, the
        // following should run on that thread pool.
        let payload_slices: Vec<&[u8]> =
            distinct_payloads.par_iter().map(|p| p.as_slice()).collect();
        SignatureProjective::par_verify_distinct_aggregated(
            &aggregate_pubkeys,
            &aggregate_signature,
            &payload_slices,
        )
        .is_ok()
    };

    measure.stop();
    stats
        .fn_verify_votes_optimistic_stats
        .increment(measure.as_us())
        .unwrap();
    verified
}

// Assuming that sigverifier's dedicated thread pool was used to call this function, the
// following should run on that thread pool.
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
fn aggregate_signatures(votes: &[VoteToVerify]) -> Result<SignatureProjective, BlsError> {
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
    votes: &[VoteToVerify],
    stats: &mut SigVerifyVoteStats,
) -> (Vec<Vec<u8>>, Result<Vec<PubkeyProjective>, BlsError>) {
    let mut grouped_votes: HashMap<&Vote, Vec<&BlsPubkey>> = HashMap::new();

    for v in votes {
        grouped_votes
            .entry(&v.vote_message.vote)
            .or_default()
            .push(&v.bls_pubkey);
    }

    stats
        .distinct_votes_stats
        .increment(grouped_votes.len() as u64)
        .unwrap();

    // Assuming that sigverifier's dedicated thread pool was used to call this function, the
    // following should run on that thread pool.
    let (distinct_payloads, distinct_pubkeys_results): (Vec<_>, Vec<_>) = grouped_votes
        .into_par_iter()
        .map(|(vote, pubkeys)| {
            (
                bincode::serialize(vote).unwrap(),
                // TODO(sam): https://github.com/anza-xyz/alpenglow/issues/708
                // should improve public key aggregation drastically (more than 80%)
                PubkeyProjective::par_aggregate(pubkeys.into_par_iter()),
            )
        })
        .unzip();
    let aggregate_pubkeys_result = distinct_pubkeys_results.into_iter().collect();

    (distinct_payloads, aggregate_pubkeys_result)
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
fn verify_individual_votes(
    votes_to_verify: Vec<VoteToVerify>,
    stats: &mut SigVerifyVoteStats,
) -> Vec<VoteToVerify> {
    let mut measure = Measure::start("verify_individual_votes");

    // Assuming that sigverifier's dedicated thread pool was used to call this function, the
    // following should run on that thread pool.
    let verified_votes = votes_to_verify
        .into_par_iter()
        .filter_map(|vote| vote.verify().then_some(vote))
        .collect();

    measure.stop();
    stats
        .fn_verify_individual_votes_stats
        .increment(measure.as_us())
        .unwrap();
    verified_votes
}
