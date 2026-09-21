use {
    crate::{
        bls_sigverifier::SigVerifierChannels,
        errors::SigVerifyVoteError,
        stats::{SigVerifyVoteStats, VoteSenderStats, VoteVerificationStats},
        unverified_votes_batch::{UnverifiedBatch, UnverifiedVotePayload},
        verified_batch::VerifiedBatch,
    },
    agave_votor_messages::wire::VotePayloadToSign,
    agave_votor_transport::endpoint::BanSender,
    rayon::{
        ThreadPool,
        iter::{IntoParallelRefMutIterator, ParallelIterator},
    },
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_measure::measure::Measure,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, epoch_stakes::BLSPubkeyToRankMap},
    std::{collections::HashMap, sync::Arc},
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
}

/// To avoid having to allocate memory for `VerifiedBatch`, this enum exists to reuse the memory
/// for the `UnverifiedBatch` when it is verified and a `VerifiedBatch` is produced.
enum BatchState {
    Unverified(UnverifiedBatch),
    Verified {
        batch: Option<VerifiedBatch>,
        num_votes_to_sigverify: usize,
        stats: VoteVerificationStats,
    },
    Empty,
}

impl BatchState {
    fn push(&mut self, payload: UnverifiedVotePayload, sender_vote_account_pubkey: Pubkey) {
        if let Self::Unverified(b) = self {
            b.push(payload, sender_vote_account_pubkey);
        }
    }

    fn verify(&mut self, max_validators: usize, ban_sender: &BanSender, thread_pool: &ThreadPool) {
        if let Self::Unverified(unverified_batch) = self {
            let num_votes_to_sigverify = unverified_batch.len();
            let (verified_batch, stats) =
                unverified_batch.verify(max_validators, ban_sender, thread_pool);
            *self = Self::Verified {
                batch: verified_batch,
                num_votes_to_sigverify,
                stats,
            }
        }
    }

    fn process(
        &mut self,
        root_bank: &Bank,
        leader_schedule: &LeaderScheduleCache,
        my_pubkey: &Pubkey,
        channels: &SigVerifierChannels,
        sender_stats: &mut VoteSenderStats,
        vote_stats: &mut SigVerifyVoteStats,
    ) -> Result<(), SigVerifyVoteError> {
        let mut batch = Self::Empty;
        std::mem::swap(&mut batch, self);
        if let Self::Verified {
            batch,
            num_votes_to_sigverify,
            stats,
        } = batch
        {
            vote_stats.votes_to_sig_verify += num_votes_to_sigverify;
            vote_stats.vote_verification_stats.merge(stats);
            if let Some(batch) = batch {
                batch.process_and_send(
                    root_bank,
                    leader_schedule,
                    my_pubkey,
                    channels,
                    sender_stats,
                )?;
            }
        }
        Ok(())
    }
}

/// Verifies votes and sends the verified votes to the consensus pool; and sends the desired subset
/// to rewards container and repair.
///
/// Any vote that fails fallback individual signature verification will have its sender banlisted.
pub(super) fn verify_and_send_votes(
    unverified_votes: &mut HashMap<VotePayloadToSign, Batch>,
    root_bank: &Bank,
    my_pubkey: &Pubkey,
    leader_schedule: &LeaderScheduleCache,
    ban_sender: &BanSender,
    thread_pool: &ThreadPool,
    channels: &SigVerifierChannels,
) -> Result<SigVerifyVoteStats, SigVerifyVoteError> {
    let mut measure = Measure::start("verify_and_send_votes");
    let mut stats = SigVerifyVoteStats::default();
    if unverified_votes.is_empty() {
        return Ok(stats);
    }
    stats
        .distinct_votes_stats
        .add_sample(unverified_votes.len() as u64);

    thread_pool.install(|| {
        unverified_votes.par_iter_mut().for_each(|(_, batch)| {
            batch
                .batch_state
                .verify(batch.rank_map.len(), ban_sender, thread_pool);
        });
    });

    let mut sender_stats = VoteSenderStats::default();
    for batch in unverified_votes.values_mut() {
        batch.batch_state.process(
            root_bank,
            leader_schedule,
            my_pubkey,
            channels,
            &mut sender_stats,
            &mut stats,
        )?;
    }
    stats.senders.merge(sender_stats);
    measure.stop();
    stats
        .fn_verify_and_send_votes_stats
        .add_sample(measure.as_us());
    Ok(stats)
}
