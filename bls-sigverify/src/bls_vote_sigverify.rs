#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    crate::{
        bls_sigverifier::{BAN_TIMEOUT, NUM_SLOTS_FOR_VERIFY, SigVerifierChannels},
        errors::SigVerifyVoteError,
        rewards::{AddVoteMessage, rewards_wants_vote},
        sig_verified_messages::{SigVerifiedBatch, VoteAggregate},
        stats::SigVerifyVoteStats,
        utils::{
            send_votes_to_metrics, send_votes_to_pool, send_votes_to_repair, send_votes_to_rewards,
        },
    },
    agave_votor_messages::{
        consensus_message::VoteMessage,
        metric_types::ConsensusMetricsEvent,
        unverified_vote_message::UnverifiedVoteMessage,
        vote::Vote,
        wire::{VotePayloadToSign, get_vote_payload_to_sign},
    },
    log::info,
    rayon::{
        ThreadPool, current_thread_index,
        iter::{Either, IntoParallelIterator, IntoParallelRefIterator, ParallelIterator},
    },
    solana_bls_signatures::{
        BlsError, PreparedHashedMessage,
        pubkey::{PopVerified, PubkeyAffine as BlsPubkeyAffine, PubkeyProjective, VerifySignature},
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

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
struct VerifiedVotePayload {
    vote_aggregate: VoteAggregate,
    sender_vote_account_pubkeys: Vec<Pubkey>,
}

/// [`VoteMessage`] along with other information needed to sig verify it.
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
#[derive(Clone, Debug)]
pub(super) struct UnverifiedVotePayload {
    pub vote_message: UnverifiedVoteMessage,
    pub sender_bls_pubkey: PopVerified<BlsPubkeyAffine>,
    pub sender_vote_account_pubkey: Pubkey,
    pub sender_identity_pubkey: Pubkey,
    pub prepared_payload: Option<Arc<PreparedHashedMessage>>,
}

impl UnverifiedVotePayload {
    fn verify(self, root_bank: &Bank) -> Option<VerifiedVotePayload> {
        let is_verified = if let Some(prepared_payload) = self.prepared_payload.as_deref() {
            self.sender_bls_pubkey
                .verify_signature_prepared(&self.vote_message.signature, prepared_payload)
                .is_ok()
        } else {
            let payload =
                get_vote_payload_to_sign(self.vote_message.vote, self.vote_message.shred_version);
            self.sender_bls_pubkey
                .verify_signature(&self.vote_message.signature, &payload)
                .is_ok()
        };
        let vote_msg = VoteMessage {
            vote: self.vote_message.vote,
            signature: self.vote_message.signature,
            rank: self.vote_message.rank,
        };
        let vote_aggregate = VoteAggregate::new_from_verified_vote(root_bank, vote_msg);
        is_verified.then_some(VerifiedVotePayload {
            vote_aggregate,
            sender_vote_account_pubkeys: vec![self.sender_vote_account_pubkey],
        })
    }
}

/// Verifies votes and sends the verified votes to the consensus pool; and sends the desired subset
/// to rewards container and repair.
///
/// Any vote that fails fallback individual signature verification will have its sender banlisted.
pub(super) fn verify_and_send_votes(
    unverified_votes: HashMap<VotePayloadToSign, Vec<UnverifiedVotePayload>>,
    root_bank: &Bank,
    cluster_info: &ClusterInfo,
    leader_schedule: &LeaderScheduleCache,
    banlist: &SimpleQosBanlist,
    thread_pool: &ThreadPool,
    channels: &SigVerifierChannels,
) -> Result<SigVerifyVoteStats, SigVerifyVoteError> {
    let mut measure = Measure::start("verify_and_send_votes");
    let mut stats = SigVerifyVoteStats::default();
    if unverified_votes.is_empty() {
        return Ok(stats);
    }
    stats
        .distinct_votes_stats
        .add_sample(unverified_votes.len() as u64);

    for (vote_payload_to_sign, unverified_votes) in unverified_votes {
        stats.votes_to_sig_verify += unverified_votes.len() as u64;
        let verified_votes = verify_votes(
            root_bank,
            vote_payload_to_sign,
            unverified_votes,
            &mut stats,
            banlist,
            thread_pool,
        );
        stats.sig_verified_votes += verified_votes.len() as u64;

        let (votes_for_pool, msgs_for_repair, msg_for_reward, msg_for_metrics) =
            process_verified_votes(verified_votes, root_bank, cluster_info, leader_schedule);

        send_votes_to_pool(votes_for_pool, &channels.channel_to_pool, &mut stats)?;
        send_votes_to_repair(msgs_for_repair, &channels.channel_to_repair, &mut stats)?;
        send_votes_to_rewards(msg_for_reward, &channels.channel_to_reward, &mut stats)?;
        send_votes_to_metrics(msg_for_metrics, &channels.channel_to_metrics, &mut stats)?;
    }

    measure.stop();
    stats
        .fn_verify_and_send_votes_stats
        .add_sample(measure.as_us());
    Ok(stats)
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
    SigVerifiedBatch,
    HashMap<Pubkey, Vec<Slot>>,
    AddVoteMessage,
    Vec<ConsensusMetricsEvent>,
) {
    let mut votes_for_reward = Vec::with_capacity(verified_votes.len());
    let mut msgs_for_repair = HashMap::new();
    let mut votes_for_pool = Vec::with_capacity(verified_votes.len());
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
        votes_for_pool.push(payload.vote_aggregate);
    }
    let msgs_for_repair = msgs_for_repair
        .into_iter()
        .map(|(pubkey, mut slots)| {
            slots.sort_unstable();
            slots.dedup();
            (pubkey, slots)
        })
        .collect();
    let votes_for_pool = SigVerifiedBatch::Votes(votes_for_pool);
    (
        votes_for_pool,
        msgs_for_repair,
        AddVoteMessage {
            votes: votes_for_reward,
        },
        votes_for_metrics,
    )
}

/// Sig verifies `unverified_votes` and returns a `Vec` of votes that passed verification.
fn verify_votes(
    root_bank: &Bank,
    vote_payload_to_sign: VotePayloadToSign,
    mut unverified_votes: Vec<UnverifiedVotePayload>,
    stats: &mut SigVerifyVoteStats,
    banlist: &SimpleQosBanlist,
    thread_pool: &ThreadPool,
) -> Vec<VerifiedVotePayload> {
    // Filter votes too far in the future.
    if vote_payload_to_sign.slot() > root_bank.slot().saturating_add(NUM_SLOTS_FOR_VERIFY) {
        stats.too_far_in_future += unverified_votes.len() as u64;
        return vec![];
    }

    // Try optimistic verification - fast to verify, but cannot identify invalid votes
    let optimistic_result = verify_votes_optimistic(
        vote_payload_to_sign,
        &mut unverified_votes,
        stats,
        thread_pool,
    );
    if let Some(aggregate_signature) = optimistic_result {
        let Some(vote_aggregate) = VoteAggregate::new_from_verified_votes(
            root_bank,
            vote_payload_to_sign,
            unverified_votes.iter().map(|v| v.vote_message.rank),
            aggregate_signature,
        ) else {
            return vec![];
        };
        return vec![VerifiedVotePayload {
            vote_aggregate,
            sender_vote_account_pubkeys: unverified_votes
                .into_iter()
                .map(|v| v.sender_vote_account_pubkey)
                .collect(),
        }];
    }

    // Fallback to individual verification
    let ((verified_votes, invalid_remote_pubkeys), time_us) = measure_us!(verify_individual_votes(
        root_bank,
        unverified_votes,
        thread_pool
    ));
    for sender_identity_pubkey in invalid_remote_pubkeys {
        if banlist.ban(sender_identity_pubkey, BAN_TIMEOUT) {
            stats.already_banned += 1;
        } else {
            info!(
                "bls_vote_sigverify: banned sender={sender_identity_pubkey} due to failed \
                 verification"
            );
        }
    }
    stats.fn_verify_individual_votes_stats.add_sample(time_us);

    verified_votes
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
#[must_use]
fn verify_votes_optimistic(
    vote_payload_to_sign: VotePayloadToSign,
    unverified_votes: &mut Vec<UnverifiedVotePayload>,
    stats: &mut SigVerifyVoteStats,
    thread_pool: &ThreadPool,
) -> Option<SignatureProjective> {
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
    let (signature_result, (prepared_hash_msg, pubkey_result)) = thread_pool.join(
        || aggregate_signatures(unverified_votes),
        || aggregate_pubkeys_by_payload(vote_payload_to_sign, unverified_votes),
    );

    let Ok(aggregate_signature) = signature_result else {
        return None;
    };

    let Ok(aggregate_pubkey) = pubkey_result else {
        return None;
    };

    let verified = aggregate_pubkey
        .verify_signature_prepared(&aggregate_signature, &prepared_hash_msg)
        .is_ok();

    measure.stop();
    stats
        .fn_verify_votes_optimistic_stats
        .add_sample(measure.as_us());
    if verified {
        Some(aggregate_signature)
    } else {
        let prepared_hash_msg = Arc::new(prepared_hash_msg);
        for unverified_vote in unverified_votes {
            unverified_vote.prepared_payload = Some(prepared_hash_msg.clone());
        }
        None
    }
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
fn aggregate_signatures(votes: &[UnverifiedVotePayload]) -> Result<SignatureProjective, BlsError> {
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

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
fn aggregate_pubkeys_by_payload(
    vote_payload_to_sign: VotePayloadToSign,
    votes: &[UnverifiedVotePayload],
) -> (
    PreparedHashedMessage,
    Result<PopVerified<PubkeyProjective>, BlsError>,
) {
    debug_assert!(current_thread_index().is_some());
    let serialized_vote = wincode::serialize(&vote_payload_to_sign).unwrap();
    let prepared_hash_msg = PreparedHashedMessage::new(&serialized_vote);
    // converting aggregate pubkey to `PopVerified` is safe here
    // since the pubkeys are all PoP verified in the vote account
    let pubkey =
        PubkeyProjective::par_aggregate(votes.into_par_iter().map(|v| &v.sender_bls_pubkey))
            .map(|agg| unsafe { PopVerified::new_unchecked(*agg) });
    (prepared_hash_msg, pubkey)
}

/// Verifies votes individually on a thread pool.
///
/// Returns:
/// - `Vec<VotePayload>`: votes that passed verification.
/// - `Vec<Pubkey>`: senders' identity pubkeys for votes that failed verification.
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
fn verify_individual_votes(
    root_bank: &Bank,
    unverified_votes: Vec<UnverifiedVotePayload>,
    thread_pool: &ThreadPool,
) -> (Vec<VerifiedVotePayload>, Vec<Pubkey>) {
    thread_pool.install(|| {
        unverified_votes
            .into_par_iter()
            .partition_map(|unverified_vote| {
                let sender_identity_pubkey = unverified_vote.sender_identity_pubkey;
                match unverified_vote.verify(root_bank) {
                    Some(vote) => Either::Left(vote),
                    None => Either::Right(sender_identity_pubkey),
                }
            })
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
        let shred_version = 1234;
        let vote = Vote::new_skip_vote(1);
        let vote_payload_to_sign = VotePayloadToSign::new_from_vote(vote, shred_version);
        // calling without a rayon thread pool should trigger a debug assert.
        aggregate_pubkeys_by_payload(vote_payload_to_sign, &votes)
            .1
            .unwrap();
    }
}
