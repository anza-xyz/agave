#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    crate::{
        bls_sigverifier::{BAN_TIMEOUT, NUM_SLOTS_FOR_VERIFY, SigVerifierChannels},
        errors::SigVerifyVoteError,
        rewards::RewardVoteMessage,
        stats::SigVerifyVoteStats,
        utils::{
            send_votes_to_metrics, send_votes_to_pool, send_votes_to_repair, send_votes_to_rewards,
        },
    },
    agave_votor_messages::{
        consensus_message::VoteMessage,
        metric_types::ConsensusMetricsEvent,
        sig_verified_messages::SigVerifiedBatch,
        unverified_vote_message::UnverifiedVoteMessage,
        vote::Vote,
        wire::{VotePayloadToSign, get_vote_payload_to_sign},
    },
    log::info,
    rayon::{
        ThreadPool,
        iter::{Either, IntoParallelIterator, ParallelIterator},
    },
    solana_bls_signatures::{
        BlsError,
        pubkey::{PopVerified, PubkeyAffine as BlsPubkeyAffine, VerifySignature},
    },
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_measure::{measure::Measure, measure_us},
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_streamer::nonblocking::simple_qos::SimpleQosBanlist,
    std::{collections::HashMap, num::NonZero},
};

fn into_vote_msg(msg: UnverifiedVoteMessage) -> VoteMessage {
    VoteMessage {
        vote: msg.vote,
        signature: msg.signature,
        rank: msg.rank,
    }
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
struct VerifiedVotePayload {
    vote_message: VoteMessage,
    sender_vote_account_pubkey: Pubkey,
    stake: NonZero<u64>,
}

/// [`VoteMessage`] along with other information needed to sig verify it.
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
#[derive(Clone, Debug)]
pub(super) struct UnverifiedVotePayload {
    pub vote_message: UnverifiedVoteMessage,
    pub sender_bls_pubkey: PopVerified<BlsPubkeyAffine>,
    pub sender_vote_account_pubkey: Pubkey,
    pub sender_identity_pubkey: Pubkey,
    pub stake: NonZero<u64>,
}

impl UnverifiedVotePayload {
    fn verify(self) -> Result<VerifiedVotePayload, BlsError> {
        let payload =
            get_vote_payload_to_sign(self.vote_message.vote, self.vote_message.shred_version);
        self.sender_bls_pubkey
            .verify_signature(&self.vote_message.signature, &payload)
            .map(|()| VerifiedVotePayload {
                vote_message: into_vote_msg(self.vote_message),
                sender_vote_account_pubkey: self.sender_vote_account_pubkey,
                stake: self.stake,
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
    let vote_slot = vote.vote_message.vote.slot();
    match vote.vote_message.vote {
        Vote::Notarize(_) | Vote::Finalize(_) | Vote::NotarizeFallback(_) => {
            msgs_for_repair
                .entry(vote.sender_vote_account_pubkey)
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
    verified_votes: Vec<VerifiedVotePayload>,
    root_bank: &Bank,
    cluster_info: &ClusterInfo,
    leader_schedule: &LeaderScheduleCache,
) -> (
    SigVerifiedBatch,
    HashMap<Pubkey, Vec<Slot>>,
    Vec<RewardVoteMessage>,
    Vec<ConsensusMetricsEvent>,
) {
    let mut votes_for_reward = Vec::with_capacity(verified_votes.len());
    let mut msgs_for_repair = HashMap::new();
    let mut votes_for_pool = Vec::with_capacity(verified_votes.len());
    let mut votes_for_metrics = Vec::with_capacity(verified_votes.len());
    for payload in verified_votes {
        if let Some(reward_vote_msg) = RewardVoteMessage::try_new(
            cluster_info,
            leader_schedule,
            root_bank.slot(),
            &payload.vote_message,
            payload.stake,
            payload.sender_vote_account_pubkey,
        ) {
            votes_for_reward.push(reward_vote_msg);
        }

        inspect_for_repair(&payload, &mut msgs_for_repair);

        votes_for_metrics.push(ConsensusMetricsEvent::Vote {
            id: payload.sender_vote_account_pubkey,
            vote: payload.vote_message.vote,
        });
        votes_for_pool.push(payload.vote_message);
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
        votes_for_reward,
        votes_for_metrics,
    )
}

/// Sig verifies `unverified_votes` and returns a `Vec` of votes that passed verification.
fn verify_votes(
    root_bank: &Bank,
    vote_payload_to_sign: VotePayloadToSign,
    unverified_votes: Vec<UnverifiedVotePayload>,
    stats: &mut SigVerifyVoteStats,
    banlist: &SimpleQosBanlist,
    thread_pool: &ThreadPool,
) -> Vec<VerifiedVotePayload> {
    debug_assert!(
        vote_payload_to_sign.slot() <= root_bank.slot().saturating_add(NUM_SLOTS_FOR_VERIFY)
    );
    // Fallback to individual verification
    let ((verified_votes, invalid_remote_pubkeys), time_us) =
        measure_us!(verify_individual_votes(unverified_votes, thread_pool));
    for (sender_identity_pubkey, error) in invalid_remote_pubkeys {
        stats.banning_validator += 1;
        if banlist.ban(sender_identity_pubkey, BAN_TIMEOUT) {
            stats.already_banned += 1;
        } else {
            info!(
                "bls_vote_sigverify: banned sender={sender_identity_pubkey} due to failed \
                 verification {error:?}"
            );
        }
    }
    stats.fn_verify_individual_votes_stats.add_sample(time_us);

    verified_votes
}

/// Verifies votes individually on a thread pool.
///
/// Returns:
/// - `Vec<VotePayload>`: votes that passed verification.
/// - `Vec<Pubkey>`: senders' identity pubkeys for votes that failed verification.
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
fn verify_individual_votes(
    unverified_votes: Vec<UnverifiedVotePayload>,
    thread_pool: &ThreadPool,
) -> (Vec<VerifiedVotePayload>, Vec<(Pubkey, BlsError)>) {
    thread_pool.install(|| {
        unverified_votes
            .into_par_iter()
            .partition_map(|unverified_vote| {
                let sender_identity_pubkey = unverified_vote.sender_identity_pubkey;
                match unverified_vote.verify() {
                    Ok(vote) => Either::Left(vote),
                    Err(e) => Either::Right((sender_identity_pubkey, e)),
                }
            })
    })
}
