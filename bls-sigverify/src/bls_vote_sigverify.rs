#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    crate::{
        bls_sigverifier::{BAN_TIMEOUT, SigVerifierChannels},
        errors::SigVerifyVoteError,
        rewards::rewards_wants_vote,
        stats::SigVerifyVoteStats,
        unverified_vote_group::{
            GroupIndex, UnverifiedVoteGroup, UnverifiedVoteGroupArena, VerifiedVotePayload,
        },
        utils::{
            send_sig_verified_batch_to_pool, send_votes_to_metrics, send_votes_to_repair,
            send_votes_to_rewards,
        },
    },
    agave_votor_messages::{
        metric_types::ConsensusMetricsEvent,
        sig_verified_messages::{SigVerifiedBatch, VoteAggregate},
        vote::Vote,
        wire::VotePayloadToSign,
    },
    agave_votor_transport::endpoint::BanSender,
    log::info,
    rayon::{ThreadPool, iter::Either},
    solana_bls_signatures::{PreparedHashedMessage, SignatureProjective, pubkey::VerifySignature},
    solana_clock::{Epoch, Slot},
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_measure::{measure::Measure, measure_us},
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, epoch_stakes::BLSPubkeyToRankMap},
    std::{collections::HashMap, sync::Arc},
};

/// Verifies votes and sends the verified votes to the consensus pool; and sends the desired subset
/// to rewards container and repair.
///
/// Any vote that fails fallback individual signature verification will have its sender banlisted.
pub(super) fn verify_and_send_votes(
    groups_arena: &UnverifiedVoteGroupArena,
    rank_map_cache: &HashMap<Epoch, Arc<BLSPubkeyToRankMap>>,
    root_bank: &Bank,
    cluster_info: &ClusterInfo,
    leader_schedule: &LeaderScheduleCache,
    ban_sender: &BanSender,
    thread_pool: &ThreadPool,
    channels: &SigVerifierChannels,
    unverified_votes: &HashMap<VotePayloadToSign, GroupIndex>,
) -> Result<SigVerifyVoteStats, SigVerifyVoteError> {
    let mut measure = Measure::start("verify_and_send_votes");
    let mut stats = SigVerifyVoteStats::default();
    if unverified_votes.is_empty() {
        return Ok(stats);
    }
    stats
        .distinct_votes_stats
        .add_sample(unverified_votes.len() as u64);

    // TODO: this should be run in parallel!
    for (vote_payload_to_sign, group_ind) in unverified_votes {
        let group = groups_arena.get(*group_ind);
        stats.votes_to_sig_verify += group.len() as u64;
        let vote_slot = vote_payload_to_sign.slot();
        let vote_epoch = root_bank.epoch_schedule().get_epoch(vote_slot);
        let rank_map = rank_map_cache
            .get(&vote_epoch)
            .expect("rank map should exist");
        let verified_votes = verify_votes(
            rank_map,
            *vote_payload_to_sign,
            group,
            &mut stats,
            ban_sender,
            thread_pool,
        );

        let (sig_verified_batch, msgs_for_repair, msg_for_reward, msg_for_metrics) =
            process_verified_votes(verified_votes, root_bank, cluster_info, leader_schedule);

        send_sig_verified_batch_to_pool(sig_verified_batch, &channels.channel_to_pool, &mut stats)?;
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
    Vec<VoteAggregate>,
    Vec<ConsensusMetricsEvent>,
) {
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
    let sig_verified_batch = SigVerifiedBatch::Votes(vote_aggregates_for_pool);
    (
        sig_verified_batch,
        msgs_for_repair,
        votes_for_reward,
        votes_for_metrics,
    )
}

/// Sig verifies `unverified_votes` and returns a `Vec` of votes that passed verification.
fn verify_votes(
    rank_map: &BLSPubkeyToRankMap,
    vote_payload_to_sign: VotePayloadToSign,
    group: &UnverifiedVoteGroup,
    stats: &mut SigVerifyVoteStats,
    ban_sender: &BanSender,
    thread_pool: &ThreadPool,
) -> Vec<VerifiedVotePayload> {
    // Try optimistic verification - fast to verify, but cannot identify invalid votes
    let res = verify_votes_optimistic(thread_pool, rank_map, &vote_payload_to_sign, group, stats);

    match res {
        Either::Left(signature) => {
            stats.optimistic_verification_succeeded += 1;
            stats.optimistic_batch.add_sample(group.len() as u64);
            vec![group.to_verified_vote_payload(rank_map, vote_payload_to_sign, signature)]
        }
        Either::Right(prepared_hash_msg) => {
            // Fallback to individual verification
            stats.optimistic_verification_failed += 1;
            let ((verified_votes, invalid_remote_pubkeys), time_us) =
                measure_us!(group.verify_individual_votes(
                    rank_map,
                    vote_payload_to_sign,
                    prepared_hash_msg,
                    thread_pool
                ));
            stats.num_individual_verified += verified_votes.len() as u64;
            for (sender_identity_pubkey, error) in invalid_remote_pubkeys {
                stats.banning_validator += 1;
                ban_sender.ban(sender_identity_pubkey, BAN_TIMEOUT);
                info!(
                    "bls_vote_sigverify: banned sender={sender_identity_pubkey} due to failed \
                     verification {error:?}"
                );
            }
            stats.fn_verify_individual_votes_stats.add_sample(time_us);
            verified_votes
        }
    }
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
    thread_pool: &ThreadPool,
    rank_map: &BLSPubkeyToRankMap,
    vote_payload_to_sign: &VotePayloadToSign,
    group: &UnverifiedVoteGroup,
    stats: &mut SigVerifyVoteStats,
) -> Either<SignatureProjective, PreparedHashedMessage> {
    group.debug_assert_unique();

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
        || group.aggregate_signatures(),
        || group.aggregate_pubkeys_by_payload(rank_map, vote_payload_to_sign),
    );

    let Ok(aggregate_signature) = signature_result else {
        return Either::Right(prepared_hash_msg);
    };

    let Ok(aggregate_pubkey) = pubkey_result else {
        return Either::Right(prepared_hash_msg);
    };

    let verified = aggregate_pubkey
        .verify_signature_prepared(&aggregate_signature, &prepared_hash_msg)
        .is_ok();

    measure.stop();
    stats
        .fn_verify_votes_optimistic_stats
        .add_sample(measure.as_us());
    if verified {
        Either::Left(aggregate_signature)
    } else {
        Either::Right(prepared_hash_msg)
    }
}
