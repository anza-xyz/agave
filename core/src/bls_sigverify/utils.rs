use {
    super::{errors::SigVerifyVoteError, stats::SigVerifyVoteStats},
    crate::{
        bls_sigverify::{errors::SigVerifyCertError, stats::SigVerifyCertStats},
        cluster_info_vote_listener::VerifiedVoterSlotsSender,
    },
    agave_votor::consensus_metrics::{ConsensusMetricsEvent, ConsensusMetricsEventSender},
    agave_votor_messages::{
        consensus_message::ConsensusMessage, reward_certificate::AddVoteMessage,
    },
    crossbeam_channel::{Sender, TrySendError},
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    std::{collections::HashMap, time::Instant},
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

pub(super) fn send_votes_to_pool(
    votes: Vec<ConsensusMessage>,
    channel: &Sender<Vec<ConsensusMessage>>,
    stats: &mut SigVerifyVoteStats,
) -> Result<(), SigVerifyVoteError> {
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

pub(super) fn send_certs_to_pool(
    messages: Vec<ConsensusMessage>,
    channel_to_pool: &Sender<Vec<ConsensusMessage>>,
    stats: &mut SigVerifyCertStats,
) -> Result<(), SigVerifyCertError> {
    if messages.is_empty() {
        return Ok(());
    }
    let len = messages.len();
    match channel_to_pool.try_send(messages) {
        Ok(()) => {
            stats.pool_sent += len as u64;
            Ok(())
        }
        Err(TrySendError::Full(_)) => {
            stats.pool_channel_full += 1;
            Ok(())
        }
        Err(TrySendError::Disconnected(_)) => {
            Err(SigVerifyCertError::ConsensusPoolChannelDisconnected)
        }
    }
}
