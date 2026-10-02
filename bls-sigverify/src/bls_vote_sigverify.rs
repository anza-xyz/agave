use {
    crate::{
        bls_sigverifier::SigVerifierChannels,
        bls_vote_sigverify::batch::Batch,
        errors::SigVerifyVoteError,
        stats::{SigVerifyVoteStats, VoteSenderStats, VoteVerificationStats},
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

pub(crate) mod batch;

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

    let (votes_to_verify, vote_verification_stats) = thread_pool.install(|| {
        unverified_votes
            .par_iter_mut()
            .fold(
                || (Saturating(0), VoteVerificationStats::default()),
                |(mut total_votes_to_verify, mut vote_verification_stats), (_, batch)| {
                    let (votes_to_verify, stats) = batch.verify(ban_sender, thread_pool);
                    total_votes_to_verify += votes_to_verify;
                    vote_verification_stats.merge(stats);
                    (total_votes_to_verify, vote_verification_stats)
                },
            )
            .reduce(
                || (Saturating(0), VoteVerificationStats::default()),
                |mut left, right| {
                    left.0 += right.0;
                    left.1.merge(right.1);
                    left
                },
            )
    });

    stats.votes_to_sig_verify += votes_to_verify;
    stats.vote_verification_stats.merge(vote_verification_stats);
    let mut sender_stats = VoteSenderStats::default();
    for (_, batch) in unverified_votes.drain() {
        batch.process(
            root_bank,
            leader_schedule,
            my_pubkey,
            channels,
            &mut sender_stats,
        )?;
    }
    stats.senders.merge(sender_stats);
    measure.stop();
    stats
        .fn_verify_and_send_votes_stats
        .add_sample(measure.as_us());
    Ok(stats)
}
