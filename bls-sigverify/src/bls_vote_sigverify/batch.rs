use {
    crate::{
        bls_sigverifier::SigVerifierChannels,
        errors::SigVerifyVoteError,
        stats::{VoteSenderStats, VoteVerificationStats},
        unverified_votes_batch::{UnverifiedBatch, UnverifiedVotePayload},
        verified_batch::VerifiedBatch,
    },
    agave_votor_messages::wire::VotePayloadToSign,
    agave_votor_transport::endpoint::BanSender,
    rayon::ThreadPool,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
};

pub(crate) struct Batch {
    batch_state: BatchState,
}

impl Batch {
    pub(crate) fn new(
        vote_payload_to_sign: VotePayloadToSign,
        batch: Vec<UnverifiedVotePayload>,
        sender_vote_account_pubkeys: Vec<Pubkey>,
        max_validators: usize,
    ) -> Self {
        let unverified_batch = UnverifiedBatch::new(
            vote_payload_to_sign,
            batch,
            sender_vote_account_pubkeys,
            max_validators,
        );
        let batch_state = BatchState::Unverified(unverified_batch);
        Self { batch_state }
    }

    pub(crate) fn push_list(
        &mut self,
        payloads: Vec<UnverifiedVotePayload>,
        sender_vote_account_pubkeys: Vec<Pubkey>,
    ) {
        self.batch_state
            .push_list(payloads, sender_vote_account_pubkeys);
    }

    #[must_use]
    pub(super) fn verify(
        &mut self,
        ban_sender: &BanSender,
        thread_pool: &ThreadPool,
    ) -> (usize, VoteVerificationStats) {
        self.batch_state.verify(ban_sender, thread_pool)
    }

    pub(super) fn process(
        &mut self,
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

    pub(crate) fn verified_stake(self) -> u64 {
        self.batch_state.verified_stake()
    }
}

/// To avoid having to allocate memory for `VerifiedBatch`, this enum exists to reuse the memory
/// for the `UnverifiedBatch` when it is verified and a `VerifiedBatch` is produced.
enum BatchState {
    Unverified(UnverifiedBatch),
    Verified(Option<VerifiedBatch>),
    Processed { verified_stake: u64 },
}

impl BatchState {
    #[must_use]
    fn verify(
        &mut self,
        ban_sender: &BanSender,
        thread_pool: &ThreadPool,
    ) -> (usize, VoteVerificationStats) {
        match self {
            Self::Unverified(unverified_batch) => {
                let num_votes_to_sigverify = unverified_batch.len();
                let (verified_batch, stats) = unverified_batch.verify(ban_sender, thread_pool);
                *self = Self::Verified(verified_batch);
                (num_votes_to_sigverify, stats)
            }
            Self::Verified(_) | Self::Processed { .. } => unreachable!("Invalid state"),
        }
    }

    fn push_list(
        &mut self,
        payloads: Vec<UnverifiedVotePayload>,
        sender_vote_account_pubkeys: Vec<Pubkey>,
    ) {
        match self {
            Self::Unverified(unverified_batch) => {
                unverified_batch.push_list(payloads, sender_vote_account_pubkeys);
            }
            Self::Verified(_) | Self::Processed { .. } => unreachable!("Invalid state"),
        }
    }

    fn process(
        &mut self,
        root_bank: &Bank,
        leader_schedule: &LeaderScheduleCache,
        my_pubkey: &Pubkey,
        channels: &SigVerifierChannels,
        sender_stats: &mut VoteSenderStats,
    ) -> Result<(), SigVerifyVoteError> {
        match self {
            Self::Verified(batch) => {
                match batch.take() {
                    None => {
                        *self = Self::Processed { verified_stake: 0 };
                    }
                    Some(batch) => {
                        let verified_stake = batch.verified_stake();
                        batch.process_and_send(
                            root_bank,
                            leader_schedule,
                            my_pubkey,
                            channels,
                            sender_stats,
                        )?;
                        *self = Self::Processed {
                            verified_stake: verified_stake.get(),
                        };
                    }
                }
                Ok(())
            }
            Self::Unverified(_) | Self::Processed { .. } => unreachable!("Invalid state"),
        }
    }

    fn verified_stake(self) -> u64 {
        match self {
            Self::Processed { verified_stake } => verified_stake,
            Self::Unverified(_) | Self::Verified(_) => unreachable!("Invalid state"),
        }
    }
}
