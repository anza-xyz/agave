#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    crate::{
        bls_sigverifier::SigVerifierChannels,
        errors::SigVerifyVoteError,
        rewards::rewards_wants_vote,
        stats::VoteSenderStats,
        utils::{
            send_sig_verified_batch_to_pool, send_votes_to_metrics, send_votes_to_repair,
            send_votes_to_rewards,
        },
    },
    agave_votor_messages::{
        metric_types::ConsensusMetricsEvent,
        sig_verified_messages::{SigVerifiedBatch, VoteAggregate},
        vote::Vote,
    },
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    std::{collections::HashMap, sync::Arc},
};

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) struct VerifiedBatch {
    vote: Vote,
    aggregates: Vec<VoteAggregate>,
    sender_vote_account_pubkeys: Arc<Vec<Pubkey>>,
}

impl VerifiedBatch {
    pub(crate) fn new(
        vote: Vote,
        aggregates: Vec<VoteAggregate>,
        sender_vote_account_pubkeys: Vec<Pubkey>,
    ) -> Self {
        Self {
            vote,
            aggregates,
            sender_vote_account_pubkeys: Arc::new(sender_vote_account_pubkeys),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.aggregates.len()
    }

    pub(crate) fn process_and_send(
        self,
        root_bank: &Bank,
        leader_schedule: &LeaderScheduleCache,
        my_pubkey: &Pubkey,
        channels: &SigVerifierChannels,
        stats: &mut VoteSenderStats,
    ) -> Result<(), SigVerifyVoteError> {
        if rewards_wants_vote(my_pubkey, leader_schedule, root_bank.slot(), &self.vote) {
            send_votes_to_rewards(
                my_pubkey,
                self.aggregates.clone(),
                &channels.channel_to_reward,
                stats,
            );
        }
        send_sig_verified_batch_to_pool(
            my_pubkey,
            SigVerifiedBatch::Votes(self.aggregates),
            &channels.channel_to_pool,
            stats,
        )?;
        match self.vote {
            Vote::Notarize(_) | Vote::Finalize(_) | Vote::NotarizeFallback(_) => {
                let vote_slot = self.vote.slot();
                let repair_msg =
                    HashMap::from([(vote_slot, self.sender_vote_account_pubkeys.clone())]);
                send_votes_to_repair(my_pubkey, repair_msg, &channels.channel_to_repair, stats);
            }
            Vote::Skip(_) | Vote::SkipFallback(_) | Vote::Genesis(_) => (),
        }
        let metrics_msg = ConsensusMetricsEvent::Vote {
            ids: self.sender_vote_account_pubkeys,
            vote: self.vote,
        };
        send_votes_to_metrics(
            my_pubkey,
            vec![metrics_msg],
            &channels.channel_to_metrics,
            stats,
        );
        Ok(())
    }
}
