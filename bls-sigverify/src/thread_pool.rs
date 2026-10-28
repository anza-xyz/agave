use {
    crate::{
        bls_vote_sigverify::batch::Batch, sig_verified_messages::VoteAggregate,
        stats::VoteVerificationStats, unverified_votes_batch::UnverifiedVotePayload,
        verified_batch::VerifiedBatch,
    },
    agave_votor_messages::{vote::Vote, wire::VotePayloadToSign},
    agave_votor_transport::endpoint::BanSender,
    rayon::{ThreadPool, iter::Either},
    solana_bls_signatures::{
        BlsError, PreparedHashedMessage, PubkeyProjective,
        pubkey::{AddToPubkeyProjective, PopVerified, PubkeyAffine},
    },
    solana_ledger::thread_pool::{WorkerJob, WorkerPool},
    solana_pubkey::Pubkey,
    std::{
        collections::HashMap,
        num::Saturating,
        sync::{Arc, mpsc},
    },
};

enum Job {
    VerifyBatches(Box<VerifyBatchesJob>),
    VerifyVote(Box<VerifyVoteJob>),
    AggregatePubkeys(AggregatePubkeysJob),
}

impl WorkerJob for Job {
    fn run(self) {
        match self {
            Self::VerifyBatches(j) => j.run(),
            Self::VerifyVote(j) => j.run(),
            Self::AggregatePubkeys(j) => j.run(),
        }
    }
}

pub(super) struct MyWorkerPool {
    workers: WorkerPool<Job>,
    ban_sender: BanSender,
    // Batch verification still uses Rayon internally.
    thread_pool: Arc<ThreadPool>,
}

impl MyWorkerPool {
    pub(super) fn new(
        num_workers: usize,
        job_queue_capacity: usize,
        ban_sender: BanSender,
        thread_pool: Arc<ThreadPool>,
    ) -> Self {
        Self {
            workers: WorkerPool::new("bls-vote-worker", num_workers, job_queue_capacity),
            ban_sender,
            thread_pool,
        }
    }
}

struct VerifyBatchesResult {
    vote_payload_to_sign: VotePayloadToSign,
    batch: Batch,
    votes_to_verify: usize,
    stats: VoteVerificationStats,
}

struct VerifyBatchesJob {
    vote_payload_to_sign: VotePayloadToSign,
    batch: Batch,
    ban_sender: BanSender,
    worker_pool: Arc<MyWorkerPool>,
    thread_pool: Arc<ThreadPool>,
    results_sender: mpsc::Sender<VerifyBatchesResult>,
}

impl VerifyBatchesJob {
    fn run(mut self) {
        let (votes_to_verify, stats) =
            self.batch
                .verify(&self.ban_sender, &self.worker_pool, &self.thread_pool);
        // The caller may have unwound and dropped its receiver.
        let _ = self.results_sender.send(VerifyBatchesResult {
            vote_payload_to_sign: self.vote_payload_to_sign,
            batch: self.batch,
            votes_to_verify,
            stats,
        });
    }
}

pub(super) fn verify_batches(
    worker_pool: &Arc<MyWorkerPool>,
    unverified_votes: &mut HashMap<VotePayloadToSign, Batch>,
) -> (Saturating<usize>, VoteVerificationStats) {
    // Results must be unbounded: workers can finish while we are still submitting jobs.
    let (results_tx, results_rx) = mpsc::channel();
    let num_batches = unverified_votes.len();
    for (vote_payload_to_sign, batch) in unverified_votes.drain() {
        let job = VerifyBatchesJob {
            vote_payload_to_sign,
            batch,
            ban_sender: worker_pool.ban_sender.clone(),
            worker_pool: Arc::clone(worker_pool),
            thread_pool: Arc::clone(&worker_pool.thread_pool),
            results_sender: results_tx.clone(),
        };
        worker_pool.workers.send(Job::VerifyBatches(Box::new(job)));
    }
    drop(results_tx);

    let mut total_votes_to_verify = Saturating(0);
    let mut vote_verification_stats = VoteVerificationStats::default();
    for VerifyBatchesResult {
        vote_payload_to_sign,
        batch,
        votes_to_verify,
        stats,
    } in results_rx
    {
        total_votes_to_verify += votes_to_verify;
        vote_verification_stats.merge(stats);
        unverified_votes.insert(vote_payload_to_sign, batch);
    }
    assert_eq!(
        unverified_votes.len(),
        num_batches,
        "verification worker failed"
    );
    (total_votes_to_verify, vote_verification_stats)
}

#[allow(clippy::large_enum_variant)]
struct VerifyVoteResult {
    unverified_vote: UnverifiedVotePayload,
    result: Result<(VoteAggregate, Pubkey), (Pubkey, BlsError)>,
}

struct VerifyVoteJob {
    prepared_msg: Arc<PreparedHashedMessage>,
    max_validators: usize,
    unverified_vote: UnverifiedVotePayload,
    sender_vote_account_pubkey: Pubkey,
    results_sender: mpsc::Sender<VerifyVoteResult>,
}

impl VerifyVoteJob {
    fn run(self) {
        let sender_identity_pubkey = self.unverified_vote.sender_identity_pubkey;
        let result = match self
            .unverified_vote
            .verify(self.max_validators, Either::Right(&self.prepared_msg))
        {
            Ok(aggregate) => Ok((aggregate, self.sender_vote_account_pubkey)),
            Err(e) => Err((sender_identity_pubkey, e)),
        };
        let _ = self.results_sender.send(VerifyVoteResult {
            unverified_vote: self.unverified_vote,
            result,
        });
    }
}

pub(super) fn do_verify_individual_votes(
    worker_pool: &Arc<MyWorkerPool>,
    vote: Vote,
    prepared_msg: PreparedHashedMessage,
    max_validators: usize,
    unverified_votes: &mut Vec<UnverifiedVotePayload>,
    sender_vote_account_pubkeys: Vec<Pubkey>,
) -> (Option<VerifiedBatch>, Vec<(Pubkey, BlsError)>) {
    let prepared_msg = Arc::new(prepared_msg);
    // Results must be unbounded: workers can finish while we are still submitting jobs.
    let (results_tx, results_rx) = mpsc::channel();
    for (unverified_vote, sender_vote_account_pubkey) in
        unverified_votes.drain(..).zip(sender_vote_account_pubkeys)
    {
        let job = VerifyVoteJob {
            prepared_msg: Arc::clone(&prepared_msg),
            max_validators,
            unverified_vote,
            sender_vote_account_pubkey,
            results_sender: results_tx.clone(),
        };
        worker_pool.workers.send(Job::VerifyVote(Box::new(job)));
    }
    drop(results_tx);

    let mut aggregates = Vec::new();
    let mut sender_vote_account_pubkeys = Vec::new();
    let mut failed = Vec::new();
    for VerifyVoteResult {
        unverified_vote,
        result,
    } in results_rx
    {
        match result {
            Ok((aggregate, sender_vote_account_pubkey)) => {
                aggregates.push(aggregate);
                sender_vote_account_pubkeys.push(sender_vote_account_pubkey);
            }
            Err(e) => failed.push(e),
        }
        unverified_votes.push(unverified_vote);
    }
    (
        (!aggregates.is_empty())
            .then(|| VerifiedBatch::new(vote, aggregates, sender_vote_account_pubkeys)),
        failed,
    )
}

struct AggregatePubkeysJob {
    sender_bls_pubkey: PopVerified<PubkeyAffine>,
    results_sender: mpsc::Sender<AggregatePubkeysResult>,
}

impl AggregatePubkeysJob {
    fn run(self) {
        let mut accumulator = PubkeyProjective::identity();
        let result = self
            .sender_bls_pubkey
            .into_inner()
            .add_to_accumulator(&mut accumulator);
        let _ = self.results_sender.send(AggregatePubkeysResult {
            accumulator,
            result,
        });
    }
}

struct AggregatePubkeysResult {
    accumulator: PubkeyProjective,
    result: Result<(), BlsError>,
}

fn do_aggregate_pubkeys_by_payload(
    worker_pool: &Arc<MyWorkerPool>,
    votes: &[UnverifiedVotePayload],
) -> Result<PopVerified<PubkeyProjective>, BlsError> {
    debug_assert!(!votes.is_empty());
    // Results must be unbounded: workers can finish while we are still submitting jobs.
    let (results_tx, results_rx) = mpsc::channel();
    for vote in votes {
        let job = AggregatePubkeysJob {
            sender_bls_pubkey: vote.sender_bls_pubkey,
            results_sender: results_tx.clone(),
        };
        worker_pool.workers.send(Job::AggregatePubkeys(job));
    }
    let mut aggregate = PubkeyProjective::identity();
    for AggregatePubkeysResult {
        accumulator,
        result,
    } in results_rx
    {
        result?;
    }
    unimplemented!()
}
