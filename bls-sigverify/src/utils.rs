use {
    crate::{
        errors::{SigVerifyCertError, SigVerifyVoteError},
        rewards::RewardInput,
        stats::{CertsVerifierStats, VoteProcessorStats},
    },
    agave_votor_messages::{
        VerifiedVotorSlotsMessage,
        certificate::Certificate,
        metric_types::{ConsensusMetricsEvent, ConsensusMetricsEventSender},
        sig_verified_messages::{SigVerifiedBatch, VoteAggregate},
    },
    crossbeam_channel::{Sender, TrySendError},
    log::{error, info, warn},
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    solana_streamer::{evicting_sender::EvictingSender, streamer::ChannelSend},
    std::{collections::HashMap, time::Instant},
};

const REWARDS_CHANNEL: &str = "channel_to_rewards";
const METRICS_CHANNEL: &str = "channel_to_metrics";
const POOL_CHANNEL: &str = "channel_to_pool";
const REPAIR_CHANNEL: &str = "channel_to_repair";

pub(super) fn send_votes_to_metrics(
    my_pubkey: &Pubkey,
    votes: Vec<ConsensusMetricsEvent>,
    channel: &ConsensusMetricsEventSender,
    stats: &mut VoteProcessorStats,
) {
    if votes.is_empty() {
        return;
    }
    let msg = (Instant::now(), votes);
    match channel.try_send(msg) {
        Ok(()) => stats.metrics_channel_succ += 1,
        Err(TrySendError::Full(_)) => {
            stats.metrics_channel_drops += 1;
            warn!("{my_pubkey}: channel \"{METRICS_CHANNEL}\" is full, dropping msg");
        }
        Err(TrySendError::Disconnected(_)) => {
            warn!("{my_pubkey}: channel \"{METRICS_CHANNEL}\" disconnected");
        }
    }
}

pub(super) fn send_votes_to_rewards(
    my_pubkey: &Pubkey,
    votes: Vec<VoteAggregate>,
    channel: &Sender<RewardInput>,
    stats: &mut VoteProcessorStats,
) {
    if votes.is_empty() {
        return;
    }
    let msg = RewardInput::External(votes);
    match channel.try_send(msg) {
        Ok(()) => stats.rewards_channel_succ += 1,
        Err(TrySendError::Full(_)) => {
            stats.rewards_channel_drops += 1;
            warn!("{my_pubkey}: channel \"{REWARDS_CHANNEL}\" is full, dropping msg");
        }
        Err(TrySendError::Disconnected(_)) => {
            warn!("{my_pubkey}: channel \"{REWARDS_CHANNEL}\" disconnected");
        }
    }
}

/// Sends the `batch` to the consensus pool.  If the channel is full, then does a
/// blocking send.
pub(super) fn send_sig_verified_batch_to_pool(
    my_pubkey: &Pubkey,
    verified_vote_aggregates: Vec<VoteAggregate>,
    channel: &Sender<SigVerifiedBatch>,
    stats: &mut VoteProcessorStats,
) -> Result<(), SigVerifyVoteError> {
    if verified_vote_aggregates.is_empty() {
        return Ok(());
    }
    let batch = SigVerifiedBatch::Votes(verified_vote_aggregates);
    match channel.try_send(batch) {
        Ok(()) => {
            stats.pool_channel_succ += 1;
            Ok(())
        }
        Err(TrySendError::Full(msgs)) => {
            stats.pool_channel_full += 1;
            error!("{my_pubkey}: channel \"{POOL_CHANNEL}\" is full.  Doing a blocking send.");
            match channel.send(msgs) {
                Ok(()) => {
                    stats.pool_channel_reopened += 1;
                    info!("{my_pubkey}: channel \"{POOL_CHANNEL}\" has space again");
                    Ok(())
                }
                Err(_) => Err(SigVerifyVoteError::ChannelDisconnected(POOL_CHANNEL)),
            }
        }
        Err(TrySendError::Disconnected(_)) => {
            Err(SigVerifyVoteError::ChannelDisconnected(POOL_CHANNEL))
        }
    }
}

pub(super) fn send_votes_to_repair(
    my_pubkey: &Pubkey,
    votes: HashMap<Slot, Vec<Pubkey>>,
    channel: &EvictingSender<VerifiedVotorSlotsMessage>,
    stats: &mut VoteProcessorStats,
) {
    if votes.is_empty() {
        return;
    }
    match channel.try_send(votes) {
        Ok(()) => stats.repair_channel_succ += 1,
        Err(TrySendError::Full(_)) => {
            stats.repair_channel_drops += 1;
            warn!("{my_pubkey}: channel \"{REPAIR_CHANNEL}\" is full, dropping msg");
        }
        Err(TrySendError::Disconnected(_)) => {
            warn!("{my_pubkey}: channel \"{REPAIR_CHANNEL}\" disconnected");
        }
    }
}

/// Sends the `batch` to the consensus pool.  If the channel is bounded and full, then does a
/// blocking send.
pub(super) fn send_certs_to_pool(
    my_pubkey: &Pubkey,
    verified_certs: Vec<Certificate>,
    channel: &Sender<SigVerifiedBatch>,
    stats: &mut CertsVerifierStats,
) -> Result<(), SigVerifyCertError> {
    if verified_certs.is_empty() {
        return Ok(());
    }
    let batch = SigVerifiedBatch::Certificates(verified_certs);
    match channel.try_send(batch) {
        Ok(()) => {
            stats.pool_channel_succ += 1;
            Ok(())
        }
        Err(TrySendError::Full(msgs)) => {
            stats.pool_channel_full += 1;
            error!("{my_pubkey}: channel \"{POOL_CHANNEL}\" is full.  Doing a blocking send.");
            match channel.send(msgs) {
                Ok(()) => {
                    stats.pool_channel_reopened += 1;
                    info!("{my_pubkey}: channel \"{POOL_CHANNEL}\" has space again");
                    Ok(())
                }
                Err(_) => Err(SigVerifyCertError::ChannelDisconnected(POOL_CHANNEL)),
            }
        }
        Err(TrySendError::Disconnected(_)) => {
            Err(SigVerifyCertError::ChannelDisconnected(POOL_CHANNEL))
        }
    }
}
