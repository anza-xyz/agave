#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    crate::{
        bls_sigverifier::SigVerifierChannels,
        errors::SigVerifyVoteError,
        rewards::rewards_wants_vote,
        sig_verified_messages::{SigVerifiedBatch, VoteAggregate},
        stats::VoteSenderStats,
        utils::{
            send_sig_verified_batch_to_pool, send_votes_to_metrics, send_votes_to_repair,
            send_votes_to_rewards,
        },
    },
    agave_votor_messages::{VoteAccountPubkeys, metric_types::ConsensusMetricsEvent, vote::Vote},
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    std::sync::Arc,
};

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) struct VerifiedBatch {
    vote: Vote,
    aggregates: Vec<VoteAggregate>,
    sender_vote_account_pubkeys: Vec<Pubkey>,
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
            sender_vote_account_pubkeys,
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
        let pubkeys = match self.vote {
            Vote::Notarize(_) | Vote::Finalize(_) | Vote::NotarizeFallback(_) => {
                let pubkeys = Arc::new(self.sender_vote_account_pubkeys);
                let vote_slot = self.vote.slot();
                let repair_msg = (vote_slot, VoteAccountPubkeys::Shared(pubkeys.clone()));
                send_votes_to_repair(my_pubkey, repair_msg, &channels.channel_to_repair, stats);
                VoteAccountPubkeys::Shared(pubkeys)
            }
            Vote::Skip(_) | Vote::SkipFallback(_) | Vote::Genesis(_) => {
                VoteAccountPubkeys::Owned(self.sender_vote_account_pubkeys)
            }
        };
        let metrics_msg = ConsensusMetricsEvent::Vote {
            ids: pubkeys,
            vote: self.vote,
        };
        send_votes_to_metrics(my_pubkey, metrics_msg, &channels.channel_to_metrics, stats);
        Ok(())
    }
}
