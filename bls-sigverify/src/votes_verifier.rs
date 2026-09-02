#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
#[cfg(debug_assertions)]
use std::collections::HashSet;
use {
    crate::{
        MessageToVotesVerifier, VerifiedVote, bls_sigverifier::BAN_TIMEOUT,
        stats::VotesVerifierStats,
    },
    agave_math_utils::welford_stats::WelfordStats,
    agave_votor_messages::{
        consensus_message::VoteMessage, sig_verified_messages::VoteAggregate,
        unverified_vote_message::UnverifiedVoteMessage, wire::VotePayloadToSign,
    },
    agave_votor_transport::endpoint::BanSender,
    crossbeam_channel::{Receiver, Sender, select},
    log::{error, info},
    rayon::{
        ThreadPool, current_thread_index,
        iter::{Either, IntoParallelIterator, IntoParallelRefIterator, ParallelIterator},
    },
    solana_bls_signatures::{
        BlsError, PreparedHashedMessage, PubkeyProjective, SignatureProjective, VerifySignature,
        pubkey::{PopVerified, PubkeyAffine as BlsPubkeyAffine},
        signature::SignatureAffine,
    },
    solana_measure::measure_us,
    solana_pubkey::Pubkey,
    solana_runtime::epoch_stakes::BLSPubkeyToRankMap,
    std::{
        collections::HashMap,
        num::{NonZero, Saturating},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    },
};

pub(crate) struct VotesVerifier {
    exit: Arc<AtomicBool>,
    ban_sender: BanSender,
    thread_pool: Arc<ThreadPool>,
    unverified_votes_receiver: Receiver<MessageToVotesVerifier>,
    verified_votes_sender: Sender<Vec<Vec<VerifiedVote>>>,
    stats: VotesVerifierStats,
}

impl VotesVerifier {
    pub(crate) fn new(
        exit: Arc<AtomicBool>,
        unverified_votes_receiver: Receiver<MessageToVotesVerifier>,
        verified_votes_sender: Sender<Vec<Vec<VerifiedVote>>>,
        ban_sender: BanSender,
        thread_pool: Arc<ThreadPool>,
    ) -> Self {
        Self {
            exit,
            unverified_votes_receiver,
            verified_votes_sender,
            ban_sender,
            thread_pool,
            stats: VotesVerifierStats::default(),
        }
    }

    pub(crate) fn run(mut self) {
        while !self.exit.load(Ordering::Relaxed) {
            let Ok(votes_map) = self.recv() else {
                error!("votes receiver channel disconnected.  Exiting.");
                break;
            };
            let (verified_votes, verify_votes_us) = measure_us!(self.verify_votes_map(votes_map));
            if let Err(()) = self.send_verified_votes(verified_votes) {
                error!("verified votes sender channel disconnected.  Exiting.");
                break;
            }
            self.stats.iterations += 1;
            self.stats.verify_votes_us.add_sample(verify_votes_us);
            self.stats.maybe_report();
        }
    }

    fn recv(&self) -> Result<MessageToVotesVerifier, ()> {
        while !self.exit.load(Ordering::Relaxed) {
            select! {
                recv(self.unverified_votes_receiver) -> msg => {
                    return msg.map_err(|_| ())
                }
                default(Duration::from_secs(1)) => continue,
            }
        }
        Err(())
    }

    fn send_verified_votes(&self, votes: Vec<Vec<VerifiedVote>>) -> Result<(), ()> {
        self.verified_votes_sender.send(votes).map_err(|_| ())
    }

    fn verify_votes_map(
        &mut self,
        votes_map: HashMap<VotePayloadToSign, (Vec<UnverifiedVote>, Arc<BLSPubkeyToRankMap>)>,
    ) -> Vec<Vec<VerifiedVote>> {
        let num_distinct_votes = votes_map.len();
        let par_result = self.thread_pool.install(|| {
            votes_map
                .into_par_iter()
                .fold(
                    || ParResult::new(num_distinct_votes),
                    |mut par_result, (vote_payload_to_sign, (unverified_votes, rank_map))| {
                        let num_unverified_votes = unverified_votes.len();
                        let res = verify_votes_batch(
                            rank_map.len(),
                            vote_payload_to_sign,
                            unverified_votes,
                            &self.ban_sender,
                            &self.thread_pool,
                        );
                        match res {
                            BatchResult::Optimistic(vote) => {
                                let num_votes_verified = num_unverified_votes;
                                par_result.num_votes_verified += num_votes_verified;
                                par_result.verified_votes.push(vec![*vote]);
                                let batch_size = num_votes_verified;
                                par_result.optimistic_batches.add_sample(batch_size as u64);
                            }
                            BatchResult::Fallback(verified_votes) => {
                                let num_votes_verified = verified_votes.len();
                                par_result.num_votes_verified += num_votes_verified;
                                par_result.num_validators_banned +=
                                    num_unverified_votes.saturating_sub(num_votes_verified);
                                par_result.verified_votes.push(verified_votes);
                                par_result.fallback_verified += num_votes_verified;
                            }
                        }
                        par_result
                    },
                )
                .reduce(
                    || ParResult::new(num_distinct_votes),
                    |mut left,
                     ParResult {
                         mut verified_votes,
                         num_votes_verified,
                         num_validators_banned,
                         optimistic_batches,
                         fallback_verified,
                     }| {
                        left.verified_votes.append(&mut verified_votes);
                        left.num_votes_verified += num_votes_verified;
                        left.optimistic_batches.merge(optimistic_batches);
                        left.num_validators_banned += num_validators_banned;
                        left.fallback_verified += fallback_verified;
                        left
                    },
                )
        });
        self.stats.votes_verified += par_result.num_votes_verified;
        self.stats.validators_banned += par_result.num_validators_banned;
        self.stats
            .distinct_votes
            .add_sample(num_distinct_votes as u64);
        self.stats
            .optimistic_batch_sizes
            .merge(par_result.optimistic_batches);
        self.stats.fallback_verified += par_result.fallback_verified;
        par_result.verified_votes
    }
}

enum BatchResult {
    Optimistic(Box<VerifiedVote>),
    Fallback(Vec<VerifiedVote>),
}

fn verify_votes_batch(
    max_validators: usize,
    vote_payload_to_sign: VotePayloadToSign,
    unverified_votes: Vec<UnverifiedVote>,
    ban_sender: &BanSender,
    thread_pool: &ThreadPool,
) -> BatchResult {
    // no need to do optimistic verification when batch size == 1.
    if let [unverified_vote] = unverified_votes.as_slice() {
        let (verification_result, sender_identity_pubkey) = {
            let serialized_vote = wincode::serialize(&vote_payload_to_sign).unwrap();
            let prepared_hash_msg = PreparedHashedMessage::new(&serialized_vote);
            let sender_identity_pubkey = unverified_vote.sender_identity_pubkey;
            (
                unverified_vote.verify(max_validators, &prepared_hash_msg),
                sender_identity_pubkey,
            )
        };
        return match verification_result {
            Ok(verified_vote) => BatchResult::Fallback(vec![verified_vote]),
            Err(error) => {
                ban_invalid_vote_sender(ban_sender, sender_identity_pubkey, error);
                BatchResult::Fallback(vec![])
            }
        };
    }

    // Try optimistic verification - fast to verify, but cannot identify invalid votes
    let res = verify_votes_optimistic(vote_payload_to_sign, &unverified_votes, thread_pool);
    match res {
        Either::Left(signature) => {
            let vote_aggregate = VoteAggregate::new_from_verified_votes(
                max_validators,
                vote_payload_to_sign,
                unverified_votes.iter().map(|v| (v.rank, v.stake)),
                signature,
            );
            let sender_vote_account_pubkeys = unverified_votes
                .into_iter()
                .map(|v| v.sender_vote_account_pubkey)
                .collect();
            BatchResult::Optimistic(Box::new(VerifiedVote {
                vote_aggregate,
                sender_vote_account_pubkeys,
            }))
        }
        Either::Right(prepared_hash_msg) => {
            // Fallback to individual verification
            let (verified_votes, invalid_remote_pubkeys) = verify_individual_votes(
                max_validators,
                unverified_votes,
                prepared_hash_msg,
                thread_pool,
            );
            for (sender_identity_pubkey, error) in invalid_remote_pubkeys {
                ban_invalid_vote_sender(ban_sender, sender_identity_pubkey, error);
            }
            BatchResult::Fallback(verified_votes)
        }
    }
}

fn ban_invalid_vote_sender(
    ban_sender: &BanSender,
    sender_identity_pubkey: Pubkey,
    error: BlsError,
) {
    ban_sender.ban(sender_identity_pubkey, BAN_TIMEOUT);
    info!(
        "bls_vote_sigverify: banned sender={sender_identity_pubkey} due to failed verification \
         {error:?}"
    );
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
/// Attempts aggregate BLS verification across the full vote set.
///
/// This fast path aggregates all vote signatures and the public keys for each
/// distinct vote payload, minimizing the number of pairing operations needed
/// for verification. When aggregation or aggregate verification fails, the
/// caller falls back to individual vote verification so invalid votes can be
/// identified precisely.
///
/// Returns the optimistic verification outcome together with the distinct vote
/// messages and their prepared payloads, which can be reused by the fallback
/// path.
#[must_use]
fn verify_votes_optimistic(
    vote_payload_to_sign: VotePayloadToSign,
    unverified_votes: &[UnverifiedVote],
    thread_pool: &ThreadPool,
) -> Either<SignatureProjective, PreparedHashedMessage> {
    #[cfg(debug_assertions)]
    {
        let deduped = unverified_votes
            .iter()
            .map(|v| &v.vote_message)
            .collect::<HashSet<_>>();
        assert_eq!(deduped.len(), unverified_votes.len());
    }

    // For BLS verification, minimizing the expensive pairing operation is key.
    // Each BLS signature verification requires two pairings.
    //
    // However, the BLS verification formula allows us to:
    // 1. Aggregate all signatures into a single signature.
    // 2. Aggregate public keys for each unique message.
    //
    // By verifying the aggregated signature against the aggregated public keys,
    // the number of pairings required is reduced to (1 + number of distinct messages).
    let (signature_result, (prepared_hash_msg, pubkey_result)) = thread_pool.join(
        || aggregate_signatures(unverified_votes),
        || aggregate_pubkeys_by_payload(vote_payload_to_sign, unverified_votes),
    );

    let Ok(aggregate_signature) = signature_result else {
        return Either::Right(prepared_hash_msg);
    };

    let Ok(aggregate_pubkey) = pubkey_result else {
        return Either::Right(prepared_hash_msg);
    };

    let verified = aggregate_pubkey
        .verify_signature_prepared(&aggregate_signature, &prepared_hash_msg)
        .is_ok();

    if verified {
        Either::Left(aggregate_signature)
    } else {
        Either::Right(prepared_hash_msg)
    }
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
fn aggregate_signatures(votes: &[UnverifiedVote]) -> Result<SignatureProjective, BlsError> {
    debug_assert!(current_thread_index().is_some());
    let signatures = votes.par_iter().map(|v| &v.vote_message.signature);
    // TODO(sam): Currently, `par_aggregate` performs full validation
    // (on-curve + subgroup check) for every signature. Since the subgroup
    // check is expensive, we can use an `unchecked` deserialization here
    // (performing only the cheap on-curve check) and rely on a single subgroup
    // check on the final aggregated signature. This should save more than 80%
    // of the time for signature aggregation.
    SignatureProjective::par_aggregate(signatures)
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
fn aggregate_pubkeys_by_payload(
    vote_payload_to_sign: VotePayloadToSign,
    votes: &[UnverifiedVote],
) -> (
    PreparedHashedMessage,
    Result<PopVerified<PubkeyProjective>, BlsError>,
) {
    debug_assert!(current_thread_index().is_some());
    let serialized_vote = wincode::serialize(&vote_payload_to_sign).unwrap();
    let prepared_hash_msg = PreparedHashedMessage::new(&serialized_vote);
    // converting aggregate pubkey to `PopVerified` is safe here
    // since the pubkeys are all PoP verified in the vote account
    let pubkey =
        PubkeyProjective::par_aggregate(votes.into_par_iter().map(|v| &v.sender_bls_pubkey))
            .map(|agg| unsafe { PopVerified::new_unchecked(*agg) });
    (prepared_hash_msg, pubkey)
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
/// Verifies votes individually on a thread pool.
///
/// Returns:
/// - `Vec<VotePayload>`: votes that passed verification.
/// - `Vec<Pubkey>`: senders' identity pubkeys for votes that failed verification.
fn verify_individual_votes(
    max_validators: usize,
    unverified_votes: Vec<UnverifiedVote>,
    prepared_hash_msg: PreparedHashedMessage,
    thread_pool: &ThreadPool,
) -> (Vec<VerifiedVote>, Vec<(Pubkey, BlsError)>) {
    thread_pool.install(|| {
        unverified_votes
            .into_par_iter()
            .partition_map(|unverified_vote| {
                let sender_identity_pubkey = unverified_vote.sender_identity_pubkey;
                match unverified_vote.verify(max_validators, &prepared_hash_msg) {
                    Ok(vote) => Either::Left(vote),
                    Err(e) => Either::Right((sender_identity_pubkey, e)),
                }
            })
    })
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
#[derive(Clone, Debug)]
pub(super) struct UnverifiedVote {
    pub vote_message: UnverifiedVoteMessage,
    pub sender_bls_pubkey: PopVerified<BlsPubkeyAffine>,
    pub sender_vote_account_pubkey: Pubkey,
    pub sender_identity_pubkey: Pubkey,
    pub rank: u16,
    pub stake: NonZero<u64>,
}

impl UnverifiedVote {
    pub(crate) fn verify(
        &self,
        max_validators: usize,
        prepared_hashed_message: &PreparedHashedMessage,
    ) -> Result<VerifiedVote, BlsError> {
        let signature = SignatureAffine::try_from(self.vote_message.signature)?;
        self.sender_bls_pubkey
            .verify_signature_prepared(&signature, prepared_hashed_message)?;
        let vote_msg = VoteMessage {
            vote: self.vote_message.vote,
            signature,
            rank: self.rank,
            stake: self.stake,
        };
        let vote_aggregate = VoteAggregate::new_from_verified_vote(max_validators, vote_msg);
        Ok(VerifiedVote {
            vote_aggregate,
            sender_vote_account_pubkeys: vec![self.sender_vote_account_pubkey],
        })
    }
}

struct ParResult {
    verified_votes: Vec<Vec<VerifiedVote>>,
    num_votes_verified: Saturating<usize>,
    num_validators_banned: Saturating<usize>,
    optimistic_batches: WelfordStats,
    fallback_verified: Saturating<usize>,
}

impl ParResult {
    fn new(num_distinct_votes: usize) -> Self {
        Self {
            verified_votes: Vec::with_capacity(num_distinct_votes),
            num_votes_verified: Saturating(0),
            num_validators_banned: Saturating(0),
            fallback_verified: Saturating(0),
            optimistic_batches: WelfordStats::default(),
        }
    }
}

#[cfg(test)]
mod metrics_tests {
    use {
        super::*, crate::test_utils::MetricsTestContext, agave_votor_messages::vote::Vote,
        agave_votor_transport::endpoint::stub_ban_channel_for_tests, crossbeam_channel::unbounded,
        rayon::ThreadPoolBuilder,
    };

    #[test]
    fn metrics_count_optimistic_fallback_and_invalid_votes_across_batches() {
        let ctx = MetricsTestContext::new();
        // Exercise reduction with both a single worker and multiple workers.
        for threads in [1, 4] {
            let (ban_sender, mut bans) = stub_ban_channel_for_tests(128);
            let mut verifier = VotesVerifier::new(
                Arc::new(AtomicBool::new(false)),
                unbounded().1,
                unbounded().0,
                ban_sender,
                Arc::new(
                    ThreadPoolBuilder::new()
                        .num_threads(threads)
                        .build()
                        .unwrap(),
                ),
            );
            let rank_map = ctx.banks.root().get_rank_map(0).unwrap().clone();
            for batch in 1..=2 {
                let mut votes = HashMap::new();
                // Many distinct payloads force the parallel reducer to merge counters.
                // Each set includes optimistic, failed optimistic, and singleton paths.
                for group in 0..8 {
                    for (kind, count) in [(0, 3), (1, 3), (2, 1), (3, 1)] {
                        let vote = Vote::new_skip_vote(1 + group * 4 + kind);
                        let mut unverified = (0..count)
                            .map(|rank| ctx.vote(vote, rank))
                            .collect::<Vec<_>>();
                        if kind == 1 || kind == 3 {
                            unverified[0].vote_message.signature =
                                ctx.validators[0].bls_keypair.sign(b"wrong payload").into();
                        }
                        votes.insert(
                            VotePayloadToSign::new_from_vote(
                                vote,
                                ctx.cluster_info.my_shred_version(),
                            ),
                            (unverified, rank_map.clone()),
                        );
                    }
                }
                let verified = verifier.verify_votes_map(votes);
                assert_eq!(
                    verified
                        .iter()
                        .flatten()
                        .map(|v| v.vote_aggregate.num_votes())
                        .sum::<usize>(),
                    48
                );
                assert_eq!(verifier.stats.votes_verified.0, 48 * batch);
                assert_eq!(verifier.stats.fallback_verified.0, 24 * batch);
                assert_eq!(verifier.stats.validators_banned.0, 16 * batch);
                assert_eq!(verifier.stats.distinct_votes.count(), batch as u64);
                assert_eq!(verifier.stats.distinct_votes.mean::<u64>(), Some(32));
                assert_eq!(verifier.stats.distinct_votes.maximum::<u64>(), Some(32));
                assert_eq!(
                    verifier.stats.optimistic_batch_sizes.count(),
                    (8 * batch) as u64
                );
                assert_eq!(verifier.stats.optimistic_batch_sizes.mean::<u64>(), Some(3));
                assert_eq!(
                    verifier.stats.optimistic_batch_sizes.maximum::<u64>(),
                    Some(3)
                );
                let mut num_bans = 0;
                while bans.try_recv().is_ok() {
                    num_bans += 1;
                }
                assert_eq!(num_bans, 16);
            }
            assert!(verifier.verify_votes_map(HashMap::new()).is_empty());
            assert_eq!(verifier.stats.votes_verified.0, 96);
            assert_eq!(verifier.stats.fallback_verified.0, 48);
            assert_eq!(verifier.stats.validators_banned.0, 32);
            assert_eq!(verifier.stats.distinct_votes.count(), 3);
            assert_eq!(verifier.stats.distinct_votes.mean::<u64>(), Some(21));
            assert_eq!(verifier.stats.optimistic_batch_sizes.count(), 16);
        }
    }
}
