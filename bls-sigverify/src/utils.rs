use {
    crate::{
        errors::{SigVerifyCertError, SigVerifyVoteError},
        rewards::RewardInput,
        stats::{SenderStats, VoteSenderStats},
    },
    agave_votor_messages::{
        VerifiedVoterSlotsSender,
        metric_types::{ConsensusMetricsEvent, ConsensusMetricsEventSender},
        sig_verified_messages::{SigVerifiedBatch, VoteAggregate},
    },
    crossbeam_channel::{Sender, TrySendError},
    log::{error, info, warn},
    solana_clock::Slot,
    solana_pubkey::Pubkey,
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
    stats: &mut VoteSenderStats,
) {
    let len = votes.len();
    let msg = (Instant::now(), votes);
    match channel.try_send(msg) {
        Ok(()) => stats.metrics_sender.sent += len as u64,
        Err(TrySendError::Full(_)) => {
            warn!("{my_pubkey}: channel \"{METRICS_CHANNEL}\" is full, dropping msg");
            stats.metrics_sender.channel_full += 1;
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
    stats: &mut VoteSenderStats,
) {
    if votes.is_empty() {
        return;
    }
    let len = votes.len();
    let msg = RewardInput::External(votes);
    match channel.try_send(msg) {
        Ok(()) => stats.rewards_sender.sent += len as u64,
        Err(TrySendError::Full(_)) => {
            warn!("{my_pubkey}: channel \"{REWARDS_CHANNEL}\" is full, dropping msg");
            stats.rewards_sender.channel_full += 1
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
    batch: SigVerifiedBatch,
    channel: &Sender<SigVerifiedBatch>,
    stats: &mut VoteSenderStats,
) -> Result<(), SigVerifyVoteError> {
    if batch.is_empty() {
        return Ok(());
    }
    let len = batch.len();
    match channel.try_send(batch) {
        Ok(()) => {
            stats.pool_sender.sent += len as u64;
            Ok(())
        }
        Err(TrySendError::Full(msgs)) => {
            stats.pool_sender.channel_full += 1;
            error!("{my_pubkey}: channel \"{POOL_CHANNEL}\" is full.  Doing a blocking send.");
            match channel.send(msgs) {
                Ok(()) => {
                    info!("{my_pubkey}: channel \"{POOL_CHANNEL}\" has space again");
                    stats.pool_sender.sent += len as u64;
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
    channel: &VerifiedVoterSlotsSender,
    stats: &mut VoteSenderStats,
) {
    if votes.is_empty() {
        return;
    }
    match channel.try_send(votes) {
        Ok(()) => stats.repair_sender.sent += 1,
        Err(TrySendError::Full(_)) => {
            warn!("{my_pubkey}: channel \"{REPAIR_CHANNEL}\" is full, dropping msg");
            stats.repair_sender.channel_full += 1
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
    batch: SigVerifiedBatch,
    channel: &Sender<SigVerifiedBatch>,
    stats: &mut SenderStats,
) -> Result<(), SigVerifyCertError> {
    if batch.is_empty() {
        return Ok(());
    }
    let len = batch.len();
    match channel.try_send(batch) {
        Ok(()) => {
            stats.sent += len as u64;
            Ok(())
        }
        Err(TrySendError::Full(msgs)) => {
            stats.channel_full += 1;
            error!("{my_pubkey}: channel \"{POOL_CHANNEL}\" is full.  Doing a blocking send.");
            match channel.send(msgs) {
                Ok(()) => {
                    info!("{my_pubkey}: channel \"{POOL_CHANNEL}\" has space again");
                    stats.sent += len as u64;
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
