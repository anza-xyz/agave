use {
    crate::{
        bls_sigverifier::SigVerifierChannels,
        errors::SigVerifyVoteError,
        stats::{VoteSenderStats, VoteVerificationStats},
        unverified_votes_batch::{FailedVotes, UnverifiedBatch, UnverifiedVotePayload},
        verified_batch::VerifiedBatch,
    },
    agave_votor_messages::wire::VotePayloadToSign,
    agave_votor_transport::endpoint::BanSender,
    rayon::ThreadPool,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, epoch_stakes::BLSPubkeyToRankMap},
    std::sync::Arc,
};

pub(crate) struct Batch {
    batch_state: BatchState,
    rank_map: Arc<BLSPubkeyToRankMap>,
}

impl Batch {
    pub(crate) fn new(
        vote_payload_to_sign: VotePayloadToSign,
        payload: UnverifiedVotePayload,
        sender_vote_account_pubkey: Pubkey,
        rank_map: Arc<BLSPubkeyToRankMap>,
    ) -> Self {
        let unverified_batch =
            UnverifiedBatch::new(vote_payload_to_sign, payload, sender_vote_account_pubkey);
        let batch_state = BatchState::Unverified(unverified_batch);
        Self {
            batch_state,
            rank_map,
        }
    }

    pub(crate) fn rank_map(&self) -> &BLSPubkeyToRankMap {
        &self.rank_map
    }

    pub(crate) fn push(
        &mut self,
        payload: UnverifiedVotePayload,
        sender_vote_account_pubkey: Pubkey,
    ) {
        self.batch_state.push(payload, sender_vote_account_pubkey)
    }

    #[must_use]
    pub(super) fn verify(
        &mut self,
        ban_sender: &BanSender,
        thread_pool: &ThreadPool,
    ) -> (usize, VoteVerificationStats, FailedVotes) {
        self.batch_state
            .verify(self.rank_map.len(), ban_sender, thread_pool)
    }

    pub(super) fn process(
        self,
        root_bank: &Bank,
        leader_schedule: &LeaderScheduleCache,
        my_pubkey: &Pubkey,
        channels: &SigVerifierChannels,
        sender_stats: &mut VoteSenderStats,
    ) -> Result<(), SigVerifyVoteError> {
        self.batch_state.process(
            root_bank,
            leader_schedule,
            my_pubkey,
            channels,
            sender_stats,
        )
    }
}

/// To avoid having to allocate memory for `VerifiedBatch`, this enum exists to reuse the memory
/// for the `UnverifiedBatch` when it is verified and a `VerifiedBatch` is produced.
enum BatchState {
    Unverified(UnverifiedBatch),
    Verified(Option<VerifiedBatch>),
}

impl BatchState {
    fn push(&mut self, payload: UnverifiedVotePayload, sender_vote_account_pubkey: Pubkey) {
        match self {
            Self::Unverified(b) => b.push(payload, sender_vote_account_pubkey),
            Self::Verified(_) => unreachable!("Invalid state"),
        }
    }

    #[must_use]
    fn verify(
        &mut self,
        max_validators: usize,
        ban_sender: &BanSender,
        thread_pool: &ThreadPool,
    ) -> (usize, VoteVerificationStats, FailedVotes) {
        match self {
            Self::Unverified(unverified_batch) => {
                let num_votes_to_sigverify = unverified_batch.len();
                let (verified_batch, stats, failed_votes) =
                    unverified_batch.verify(max_validators, ban_sender, thread_pool);
                *self = Self::Verified(verified_batch);
                (num_votes_to_sigverify, stats, failed_votes)
            }
            Self::Verified(_) => unreachable!("Invalid state"),
        }
    }

    fn process(
        self,
        root_bank: &Bank,
        leader_schedule: &LeaderScheduleCache,
        my_pubkey: &Pubkey,
        channels: &SigVerifierChannels,
        sender_stats: &mut VoteSenderStats,
    ) -> Result<(), SigVerifyVoteError> {
        match self {
            Self::Verified(batch) => {
                if let Some(batch) = batch {
                    batch.process_and_send(
                        root_bank,
                        leader_schedule,
                        my_pubkey,
                        channels,
                        sender_stats,
                    )?;
                }
                Ok(())
            }
            Self::Unverified(_) => unreachable!("Invalid state"),
        }
    }
}
