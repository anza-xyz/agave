use {
    crate::{
        errors::{SigVerifyCertError, SigVerifyVoteError},
        stats::{SigVerifyCertStats, SigVerifyVoteStats},
    },
    agave_votor_messages::{
        VerifiedVoterSlotsSender,
        metric_types::{ConsensusMetricsEvent, ConsensusMetricsEventSender},
        reward_certificate::AddVoteMessage,
        sig_verified_messages::SigVerifiedBatch,
    },
    crossbeam_channel::{Sender, TrySendError},
    log::{error, info},
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    std::{collections::HashMap, num::Saturating, time::Instant},
};

pub(super) fn send_votes_to_metrics(
    votes: Vec<ConsensusMetricsEvent>,
    channel: &ConsensusMetricsEventSender,
    stats: &mut SigVerifyVoteStats,
) -> Result<(), SigVerifyVoteError> {
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
        Err(TrySendError::Disconnected(_)) => Err(SigVerifyVoteError::MetricsChannelDisconnected),
    }
}

pub(super) fn send_votes_to_rewards(
    msg: AddVoteMessage,
    channel: &Sender<AddVoteMessage>,
    stats: &mut SigVerifyVoteStats,
) -> Result<(), SigVerifyVoteError> {
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
        Err(TrySendError::Disconnected(_)) => Err(SigVerifyVoteError::RewardsChannelDisconnected),
    }
}

/// Sends the `batch` to the consensus pool.  If the channel is full, then does a
/// blocking send.
pub(super) fn send_votes_to_pool(
    batch: SigVerifiedBatch,
    channel: &Sender<SigVerifiedBatch>,
    stats: &mut SigVerifyVoteStats,
) -> Result<(), SigVerifyVoteError> {
    if batch.is_empty() {
        return Ok(());
    }
    let len = batch.len();
    match channel.try_send(batch) {
        Ok(()) => {
            stats.pool_sent += len as u64;
            stats.pool_outstanding_msgs = Saturating(channel.len() as u64);
            Ok(())
        }
        Err(TrySendError::Full(msgs)) => {
            stats.pool_channel_full += 1;
            error!("votes channel to consensus pool is full.  Doing a blocking send.");
            match channel.send(msgs) {
                Ok(()) => {
                    info!("votes channel to consensus pool has space again");
                    stats.pool_sent += len as u64;
                    Ok(())
                }
                Err(_) => Err(SigVerifyVoteError::ConsensusPoolChannelDisconnected),
            }
        }
        Err(TrySendError::Disconnected(_)) => {
            Err(SigVerifyVoteError::ConsensusPoolChannelDisconnected)
        }
    }
}

pub(super) fn send_votes_to_repair(
    votes: HashMap<Pubkey, Vec<Slot>>,
    channel: &VerifiedVoterSlotsSender,
    stats: &mut SigVerifyVoteStats,
) -> Result<(), SigVerifyVoteError> {
    for (pubkey, slots) in votes {
        match channel.try_send((pubkey, slots)) {
            Ok(()) => {
                stats.repair_sent += 1;
            }
            Err(TrySendError::Full(_)) => {
                stats.repair_channel_full += 1;
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err(SigVerifyVoteError::RepairChannelDisconnected);
            }
        }
    }
    Ok(())
}

/// Sends the `batch` to the consensus pool.  If the channel is bounded and full, then does a
/// blocking send.
pub(super) fn send_certs_to_pool(
    batch: SigVerifiedBatch,
    channel_to_pool: &Sender<SigVerifiedBatch>,
    stats: &mut SigVerifyCertStats,
) -> Result<(), SigVerifyCertError> {
    if batch.is_empty() {
        return Ok(());
    }
    let len = batch.len();
    match channel_to_pool.try_send(batch) {
        Ok(()) => {
            stats.pool_sent += len as u64;
            stats.pool_outstanding_msgs = Saturating(channel_to_pool.len() as u64);
            Ok(())
        }
        Err(TrySendError::Full(msgs)) => {
            stats.pool_channel_full += 1;
            error!("certs channel to consensus pool is full.  Doing a blocking send.");
            match channel_to_pool.send(msgs) {
                Ok(()) => {
                    info!("certs channel to consensus pool has space again");
                    stats.pool_sent += len as u64;
                    Ok(())
                }
                Err(_) => Err(SigVerifyCertError::ConsensusPoolChannelDisconnected),
            }
        }
        Err(TrySendError::Disconnected(_)) => {
            Err(SigVerifyCertError::ConsensusPoolChannelDisconnected)
        }
    }
}
