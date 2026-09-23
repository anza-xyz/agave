use {
    crate::{
        bls_sigverifier::SigVerifierChannels,
        errors::SigVerifyVoteError,
        stats::{SigVerifyVoteStats, VoteSenderStats, VoteVerificationStats},
        unverified_votes_batch::UnverifiedBatch,
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
    solana_runtime::bank::Bank,
    std::{collections::HashMap, num::Saturating},
};

/// Verifies votes and sends the verified votes to the consensus pool; and sends the desired subset
/// to rewards container and repair.
///
/// Any vote that fails fallback individual signature verification will have its sender banlisted.
pub(super) fn verify_and_send_votes(
    unverified_votes: &mut HashMap<VotePayloadToSign, UnverifiedBatch>,
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

    let par_result = thread_pool.install(|| {
        unverified_votes
            .par_iter_mut()
            .fold(
                ParResult::default,
                |mut par_result, (_, unverified_batch)| {
                    let num_votes_to_sigverify = unverified_batch.len();
                    let (verified_batch, stats) = unverified_batch.verify(ban_sender, thread_pool);
                    par_result.add(verified_batch, stats, num_votes_to_sigverify);
                    par_result
                },
            )
            .reduce(ParResult::default, |mut left, right| {
                left.merge(right);
                left
            })
    });
    let sender_stats = process_and_send_verified_votes(
        root_bank,
        leader_schedule,
        my_pubkey,
        channels,
        par_result.verified_votes,
    )?;
    stats.votes_to_sig_verify += par_result.num_votes_to_sigverify;
    stats
        .vote_verification_stats
        .merge(par_result.verification_stats);
    stats.senders.merge(sender_stats);

    measure.stop();
    stats
        .fn_verify_and_send_votes_stats
        .add_sample(measure.as_us());
    Ok(stats)
}

fn process_and_send_verified_votes(
    root_bank: &Bank,
    leader_schedule: &LeaderScheduleCache,
    my_pubkey: &Pubkey,
    channels: &SigVerifierChannels,
    verified_batches: Vec<VerifiedBatch>,
) -> Result<VoteSenderStats, SigVerifyVoteError> {
    let mut sender_stats = VoteSenderStats::default();
    for batch in verified_batches {
        batch.process_and_send(
            root_bank,
            leader_schedule,
            my_pubkey,
            channels,
            &mut sender_stats,
        )?;
    }
    Ok(sender_stats)
}

#[derive(Default)]
struct ParResult {
    verified_votes: Vec<VerifiedBatch>,
    verification_stats: VoteVerificationStats,
    num_votes_to_sigverify: Saturating<usize>,
}

impl ParResult {
    fn add(
        &mut self,
        verified_batch: Option<VerifiedBatch>,
        verification_stats: VoteVerificationStats,
        num_votes_to_sigverify: usize,
    ) {
        self.verification_stats.merge(verification_stats);
        self.num_votes_to_sigverify += num_votes_to_sigverify;
        if let Some(b) = verified_batch {
            self.verified_votes.push(b);
        }
    }

    fn merge(&mut self, other: Self) {
        let Self {
            mut verified_votes,
            verification_stats,
            num_votes_to_sigverify,
        } = other;
        self.verified_votes.append(&mut verified_votes);
        self.verification_stats.merge(verification_stats);
        self.num_votes_to_sigverify += num_votes_to_sigverify;
    }
}
