#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    crate::cluster_info_vote_listener::VerifiedVoterSlotsSender,
    agave_votor::{
        consensus_metrics::{ConsensusMetricsEvent, ConsensusMetricsEventSender},
        consensus_rewards,
    },
    agave_votor_messages::{
        consensus_message::{ConsensusMessage, VoteMessage},
        reward_certificate::AddVoteMessage,
        vote::Vote,
    },
    crossbeam_channel::{Sender, TrySendError},
    histogram::Histogram,
    rayon::iter::{IntoParallelIterator, IntoParallelRefIterator, ParallelIterator},
    solana_bls_signatures::{
        pubkey::{Pubkey as BlsPubkey, PubkeyProjective, VerifiablePubkey},
        signature::SignatureProjective,
        BlsError,
    },
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_measure::measure::Measure,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    std::{collections::HashMap, time::Instant},
    thiserror::Error,
};

/// Different types of errors that verifying votes can fail with.
#[derive(Debug, Error)]
#[allow(clippy::enum_variant_names)]
pub(super) enum Error {
    #[error("channel to consensus pool disconnected")]
    ConsensusPoolChannelDisconnected,
    #[error("channel to rewards container disconnected")]
    RewardsChannelDisconnected,
    #[error("channel to repair disconnected")]
    RepairChannelDisconnected,
    #[error("channel to metrics disconnected")]
    MetricsChannelDisconnected,
}

/// Struct to capture and report on stats for this module.
//
// Some fields are `pub` to facilitate testing.
#[derive(Default)]
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(super) struct Stats {
    /// Number of votes [`verify_and_send_votes`] was requested to verify the signature of.
    votes_to_sig_verify: u64,
    /// Number of votes [`verify_and_send_votes`] successfully verified the signature of.
    sig_verified_votes: u64,

    /// Number of votes sent successfully over the channel to metrics.
    metrics_sent: u64,
    /// Number of times the channel to metrics was full.
    metrics_channel_full: u64,
    /// Number of votes sent successfully over the channel to rewards.
    rewards_sent: u64,
    /// Number of times the channel to rewards was full.
    rewards_channel_full: u64,
    /// Number of votes sent successfully over the channel to consensus pool.
    pub(super) pool_sent: u64,
    /// Number of times the channel to consensus pool was full.
    pub(super) pool_channel_full: u64,
    /// Number of votes sent successfully over the channel to repair.
    repair_sent: u64,
    /// Number of times the channel to repair was full.
    repair_channel_full: u64,

    /// Stats for [`verify_and_send_votes`].
    fn_verify_and_send_votes_stats: Histogram,
    /// Stats for [`verify_votes_optimistic`].
    fn_verify_votes_optimistic_stats: Histogram,
    /// Stats for [`verify_individual_votes`].
    fn_verify_individual_votes_stats: Histogram,

    /// Stats for number of distinct votes in batches.
    pub(super) distinct_votes_stats: Histogram,
}

impl Stats {
    pub(super) fn merge(&mut self, other: Self) {
        let Self {
            votes_to_sig_verify: votes_to_verify,
            sig_verified_votes: verified_votes,
            metrics_sent,
            metrics_channel_full,
            rewards_sent,
            rewards_channel_full,
            repair_sent,
            repair_channel_full,
            pool_sent,
            pool_channel_full,
            fn_verify_and_send_votes_stats,
            fn_verify_votes_optimistic_stats,
            fn_verify_individual_votes_stats: fn_verify_individual_votes,
            distinct_votes_stats,
        } = other;
        self.votes_to_sig_verify += votes_to_verify;
        self.sig_verified_votes += verified_votes;
        self.metrics_sent += metrics_sent;
        self.metrics_channel_full += metrics_channel_full;
        self.rewards_sent += rewards_sent;
        self.rewards_channel_full += rewards_channel_full;
        self.repair_sent += repair_sent;
        self.repair_channel_full += repair_channel_full;
        self.pool_sent += pool_sent;
        self.pool_channel_full += pool_channel_full;
        self.fn_verify_and_send_votes_stats
            .merge(&fn_verify_and_send_votes_stats);
        self.fn_verify_votes_optimistic_stats
            .merge(&fn_verify_votes_optimistic_stats);
        self.fn_verify_individual_votes_stats
            .merge(&fn_verify_individual_votes);
        self.distinct_votes_stats.merge(&distinct_votes_stats);
    }

    pub(super) fn report(&self) {
        let Self {
            votes_to_sig_verify,
            sig_verified_votes,
            metrics_sent,
            metrics_channel_full,
            rewards_sent,
            rewards_channel_full,
            repair_sent,
            repair_channel_full,
            pool_sent,
            pool_channel_full,
            fn_verify_and_send_votes_stats,
            fn_verify_votes_optimistic_stats,
            fn_verify_individual_votes_stats,
            distinct_votes_stats,
        } = self;
        datapoint_info!(
            "bls_vote_sigverify_stats",
            ("votes_to_sig_verify", *votes_to_sig_verify, i64),
            ("sig_verified_votes", *sig_verified_votes, i64),
            ("metrics_sent", *metrics_sent, i64),
            ("metrics_channel_full", *metrics_channel_full, i64),
            ("rewards_sent", *rewards_sent, i64),
            ("rewards_channel_full", *rewards_channel_full, i64),
            ("repair_sent", *repair_sent, i64),
            ("repair_channel_full", *repair_channel_full, i64),
            ("pool_sent", *pool_sent, i64),
            ("pool_channel_full", *pool_channel_full, i64),
            (
                "fn_verify_and_send_votes_count",
                fn_verify_and_send_votes_stats.entries(),
                i64
            ),
            (
                "fn_verify_and_send_votes_mean",
                fn_verify_and_send_votes_stats.mean().unwrap_or(0),
                i64
            ),
            (
                "fn_verify_votes_optimistic_count",
                fn_verify_votes_optimistic_stats.entries(),
                i64
            ),
            (
                "fn_verify_votes_optimistic_mean",
                fn_verify_votes_optimistic_stats.mean().unwrap_or(0),
                i64
            ),
            (
                "fn_verify_individual_votes_count",
                fn_verify_individual_votes_stats.entries(),
                i64
            ),
            (
                "fn_verify_individual_votes_mean",
                fn_verify_individual_votes_stats.mean().unwrap_or(0),
                i64
            ),
            ("distinct_votes_count", distinct_votes_stats.entries(), i64),
            (
                "distinct_votes_mean",
                distinct_votes_stats.mean().unwrap_or(0),
                i64
            ),
        );
    }
}

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
    last_voted_slots: &mut HashMap<Pubkey, Slot>,
) -> Result<(Vec<VoteToVerify>, Stats), Error> {
    let mut measure = Measure::start("verify_and_send_votes");
    let mut stats = Stats::default();
    if votes_to_verify.is_empty() {
        return Ok((votes_to_verify, stats));
    }
    stats.votes_to_sig_verify += votes_to_verify.len() as u64;
    let verified_votes = verify_votes(votes_to_verify, &mut stats);
    stats.sig_verified_votes += verified_votes.len() as u64;

    let (votes_for_pool, msgs_for_repair, msg_for_reward, msg_for_metrics) = process_verified_votes(
        &verified_votes,
        root_bank,
        cluster_info,
        leader_schedule,
        last_voted_slots,
    );

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

fn inspect_for_repair(
    vote: &VoteToVerify,
    last_voted_slots: &mut HashMap<Pubkey, Slot>,
    msgs_for_repair: &mut HashMap<Pubkey, Vec<Slot>>,
) {
    let vote_slot = vote.vote_message.vote.slot();
    if vote.vote_message.vote.is_notarization_or_finalization() {
        last_voted_slots
            .entry(vote.pubkey)
            .and_modify(|s| *s = (*s).max(vote_slot))
            .or_insert(vote.vote_message.vote.slot());
    }

    if vote.vote_message.vote.is_notarization_or_finalization()
        || vote.vote_message.vote.is_notarize_fallback()
    {
        let slots: &mut Vec<_> = msgs_for_repair.entry(vote.pubkey).or_default();
        if !slots.contains(&vote_slot) {
            slots.push(vote_slot);
        }
    }
}

/// Processes the verified votes for various downstream services.
///
/// In particular, collects and returns the relevant messages for the consensus pool; rewards;
/// repair; and metrics;
///
/// Also updates `last_voted_slots`.
fn process_verified_votes(
    verified_votes: &[VoteToVerify],
    root_bank: &Bank,
    cluster_info: &ClusterInfo,
    leader_schedule: &LeaderScheduleCache,
    last_voted_slots: &mut HashMap<Pubkey, Slot>,
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

        inspect_for_repair(vote, last_voted_slots, &mut msgs_for_repair);

        votes_for_pool.push(ConsensusMessage::Vote(vote_message));

        votes_for_metrics.push(ConsensusMetricsEvent::Vote {
            id: vote.pubkey,
            vote: vote.vote_message.vote,
        });
    }
    (
        votes_for_pool,
        msgs_for_repair,
        AddVoteMessage {
            votes: votes_for_reward,
        },
        votes_for_metrics,
    )
}

fn send_votes_to_metrics(
    votes: Vec<ConsensusMetricsEvent>,
    channel: &ConsensusMetricsEventSender,
    stats: &mut Stats,
) -> Result<(), Error> {
    let len = votes.len();
    let msg = (Instant::now(), votes);
    match channel.try_send(msg) {
        Ok(()) => {
            stats.metrics_sent += len as u64;
            Ok(())
        }
        Err(TrySendError::Full(_)) => {
            stats.metrics_channel_full += 1;
            Ok(())
        }
        Err(TrySendError::Disconnected(_)) => Err(Error::MetricsChannelDisconnected),
    }
}

fn send_votes_to_rewards(
    msg: AddVoteMessage,
    channel: &Sender<AddVoteMessage>,
    stats: &mut Stats,
) -> Result<(), Error> {
    let len = msg.votes.len();
    match channel.try_send(msg) {
        Ok(()) => {
            stats.rewards_sent += len as u64;
            Ok(())
        }
        Err(TrySendError::Full(_)) => {
            stats.rewards_channel_full += 1;
            Ok(())
        }
        Err(TrySendError::Disconnected(_)) => Err(Error::RewardsChannelDisconnected),
    }
}

fn send_votes_to_pool(
    votes: Vec<ConsensusMessage>,
    channel: &Sender<Vec<ConsensusMessage>>,
    stats: &mut Stats,
) -> Result<(), Error> {
    let len = votes.len();
    if len == 0 {
        return Ok(());
    }
    match channel.try_send(votes) {
        Ok(()) => {
            stats.pool_sent += len as u64;
            Ok(())
        }
        Err(TrySendError::Full(_)) => {
            stats.pool_channel_full += 1;
            Ok(())
        }
        Err(TrySendError::Disconnected(_)) => Err(Error::ConsensusPoolChannelDisconnected),
    }
}

fn send_votes_to_repair(
    votes: HashMap<Pubkey, Vec<Slot>>,
    channel: &VerifiedVoterSlotsSender,
    stats: &mut Stats,
) -> Result<(), Error> {
    for (pubkey, slots) in votes {
        match channel.try_send((pubkey, slots)) {
            Ok(()) => {
                stats.repair_sent += 1;
            }
            Err(TrySendError::Full(_)) => {
                stats.repair_channel_full += 1;
            }
            Err(TrySendError::Disconnected(_)) => return Err(Error::RepairChannelDisconnected),
        }
    }
    Ok(())
}

fn verify_votes(votes_to_verify: Vec<VoteToVerify>, stats: &mut Stats) -> Vec<VoteToVerify> {
    // Try optimistic verification - fast to verify, but cannot identify invalid votes
    if verify_votes_optimistic(&votes_to_verify, stats) {
        return votes_to_verify;
    }

    // Fallback to individual verification
    verify_individual_votes(votes_to_verify, stats)
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
fn verify_votes_optimistic(votes_to_verify: &[VoteToVerify], stats: &mut Stats) -> bool {
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
        let payload_slices: Vec<&[u8]> = distinct_payloads.iter().map(|p| p.as_slice()).collect();
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
    stats: &mut Stats,
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

    let (distinct_payloads, distinct_pubkeys_results): (Vec<_>, Vec<_>) = grouped_votes
        .into_par_iter()
        .map(|(vote, pubkeys)| {
            (
                get_vote_payload(vote),
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
    stats: &mut Stats,
) -> Vec<VoteToVerify> {
    let mut measure = Measure::start("verify_individual_votes");

    let verified_votes: Vec<VoteToVerify> = votes_to_verify
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

fn get_vote_payload(vote: &Vote) -> Vec<u8> {
    bincode::serialize(vote).expect("Failed to serialize vote")
}
