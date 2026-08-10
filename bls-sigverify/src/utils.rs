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
        Err(TrySendError::Full(_)) => stats.metrics_sender.channel_full += 1,
        Err(TrySendError::Disconnected(_)) => {
            warn!("{my_pubkey}: channel \"channel_to_metrics\" disconnected");
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
        Err(TrySendError::Full(_)) => stats.rewards_sender.channel_full += 1,
        Err(TrySendError::Disconnected(_)) => {
            warn!("{my_pubkey}: channel \"channel_to_rewards\" disconnected");
        }
    }
}

/// Sends the `batch` to the consensus pool.  If the channel is full, then does a
/// blocking send.
pub(super) fn send_sig_verified_batch_to_pool(
    batch: SigVerifiedBatch,
    channel: &Sender<SigVerifiedBatch>,
    stats: &mut VoteSenderStats,
) -> Result<(), SigVerifyVoteError> {
    let channel_name = "channel_to_pool";
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
            error!("channel \"{channel_name}\" is full.  Doing a blocking send.");
            match channel.send(msgs) {
                Ok(()) => {
                    info!("channel \"{channel_name}\" has space again");
                    stats.pool_sender.sent += len as u64;
                    Ok(())
                }
                Err(_) => Err(SigVerifyVoteError::ChannelDisconnected(channel_name)),
            }
        }
        Err(TrySendError::Disconnected(_)) => {
            Err(SigVerifyVoteError::ChannelDisconnected(channel_name))
        }
    }
}

pub(super) fn send_votes_to_repair(
    my_pubkey: &Pubkey,
    votes: HashMap<Pubkey, Vec<Slot>>,
    channel: &VerifiedVoterSlotsSender,
    stats: &mut VoteSenderStats,
) {
    for (pubkey, slots) in votes {
        match channel.try_send((pubkey, slots)) {
            Ok(()) => stats.repair_sender.sent += 1,
            Err(TrySendError::Full(_)) => stats.repair_sender.channel_full += 1,
            Err(TrySendError::Disconnected(_)) => {
                warn!("{my_pubkey}: channel \"channel_to_repair\" disconnected");
                return;
            }
        }
    }
}

/// Sends the `batch` to the consensus pool.  If the channel is bounded and full, then does a
/// blocking send.
pub(super) fn send_certs_to_pool(
    batch: SigVerifiedBatch,
    channel: &Sender<SigVerifiedBatch>,
    stats: &mut SenderStats,
) -> Result<(), SigVerifyCertError> {
    let channel_name = "channel_to_pool";
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
            error!("channel \"{channel_name}\" is full.  Doing a blocking send.");
            match channel.send(msgs) {
                Ok(()) => {
                    info!("channel \"{channel_name}\" has space again");
                    stats.sent += len as u64;
                    Ok(())
                }
                Err(_) => Err(SigVerifyCertError::ChannelDisconnected(channel_name)),
            }
        }
        Err(TrySendError::Disconnected(_)) => {
            Err(SigVerifyCertError::ChannelDisconnected(channel_name))
        }
    }
}
