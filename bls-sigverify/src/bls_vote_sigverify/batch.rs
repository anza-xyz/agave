use {
    crate::{
        bls_sigverifier::SigVerifierChannels,
        errors::SigVerifyVoteError,
        stats::{VoteSenderStats, VoteVerificationStats},
        unverified_votes_batch::{UnverifiedBatch, UnverifiedVotePayload},
        verified_batch::VerifiedBatch,
    },
    agave_votor_messages::wire::VotePayloadToSign,
    rayon::ThreadPool,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    std::collections::HashSet,
};

pub(crate) struct Batch {
    batch_state: BatchState,
}

impl Batch {
    pub(crate) fn new(
        vote_payload_to_sign: VotePayloadToSign,
        batch: Vec<UnverifiedVotePayload>,
        max_validators: usize,
    ) -> Self {
        let unverified_batch = UnverifiedBatch::new(vote_payload_to_sign, batch, max_validators);
        let batch_state = BatchState::Unverified(unverified_batch);
        Self { batch_state }
    }

    pub(crate) fn push(&mut self, payloads: impl Iterator<Item = UnverifiedVotePayload>) {
        self.batch_state.push(payloads);
    }

    #[must_use]
    pub(super) fn verify(&mut self, thread_pool: &ThreadPool) -> (usize, VoteVerificationStats) {
        self.batch_state.verify(thread_pool)
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

    pub(crate) fn verified_stake_and_pubkeys_to_ban(self) -> (u64, HashSet<Pubkey>) {
        self.batch_state.verified_stake_and_pubkeys_to_ban()
    }
}

/// To avoid having to allocate memory for `VerifiedBatch`, this enum exists to reuse the memory
/// for the `UnverifiedBatch` when it is verified and a `VerifiedBatch` is produced.
enum BatchState {
    Unverified(UnverifiedBatch),
    Verified {
        batch: Option<VerifiedBatch>,
        pubkeys_to_ban: HashSet<Pubkey>,
    },
    Processed {
        verified_stake: u64,
        pubkeys_to_ban: HashSet<Pubkey>,
    },
}

impl BatchState {
    #[must_use]
    fn verify(&mut self, thread_pool: &ThreadPool) -> (usize, VoteVerificationStats) {
        match self {
            Self::Unverified(unverified_batch) => {
                let num_votes_to_sigverify = unverified_batch.len();
                let (verified_batch, pubkeys_to_ban, stats) = unverified_batch.verify(thread_pool);
                *self = Self::Verified {
                    batch: verified_batch,
                    pubkeys_to_ban,
                };
                (num_votes_to_sigverify, stats)
            }
            Self::Verified { .. } | Self::Processed { .. } => unreachable!("Invalid state"),
        }
    }

    fn push(&mut self, payloads: impl Iterator<Item = UnverifiedVotePayload>) {
        match self {
            Self::Unverified(unverified_batch) => {
                unverified_batch.push(payloads);
            }
            Self::Verified { .. } | Self::Processed { .. } => unreachable!("Invalid state"),
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
            Self::Verified {
                batch,
                pubkeys_to_ban,
            } => {
                let pubkeys = std::mem::take(pubkeys_to_ban);
                match batch.take() {
                    None => {
                        *self = Self::Processed {
                            verified_stake: 0,
                            pubkeys_to_ban: pubkeys,
                        };
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
                            pubkeys_to_ban: pubkeys,
                        };
                    }
                }
                Ok(())
            }
            Self::Unverified(_) | Self::Processed { .. } => unreachable!("Invalid state"),
        }
    }

    fn verified_stake_and_pubkeys_to_ban(self) -> (u64, HashSet<Pubkey>) {
        match self {
            Self::Processed {
                verified_stake,
                pubkeys_to_ban,
            } => (verified_stake, pubkeys_to_ban),
            Self::Unverified(_) | Self::Verified { .. } => unreachable!("Invalid state"),
        }
    }
}
