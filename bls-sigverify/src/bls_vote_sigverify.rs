#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
#[cfg(debug_assertions)]
use std::collections::HashSet;
use {
    crate::{
        bls_sigverifier::{BAN_TIMEOUT, SigVerifierChannels},
        errors::SigVerifyVoteError,
        rewards::rewards_wants_vote,
        stats::{SigVerifyVoteStats, VoteSenderStats, VoteVerificationStats},
        utils::{
            send_sig_verified_batch_to_pool, send_votes_to_metrics, send_votes_to_repair,
            send_votes_to_rewards,
        },
    },
    agave_votor_messages::{
        consensus_message::VoteMessage,
        metric_types::ConsensusMetricsEvent,
        sig_verified_messages::{SigVerifiedBatch, VoteAggregate},
        unverified_vote_message::UnverifiedVoteMessage,
        vote::Vote,
        wire::VotePayloadToSign,
    },
    agave_votor_transport::endpoint::BanSender,
    log::info,
    rayon::{
        ThreadPool, current_thread_index,
        iter::{Either, IntoParallelIterator, IntoParallelRefIterator, ParallelIterator},
    },
    solana_bls_signatures::{
        BlsError, PreparedHashedMessage, PubkeyProjective, SignatureProjective,
        pubkey::{PopVerified, PubkeyAffine as BlsPubkeyAffine, VerifySignature},
        signature::SignatureAffine,
    },
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_measure::{measure::Measure, measure_us},
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, epoch_stakes::BLSPubkeyToRankMap},
    std::{
        collections::HashMap,
        num::{NonZero, Saturating},
        sync::Arc,
    },
};

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
struct VerifiedVotePayload {
    vote_aggregate: VoteAggregate,
    sender_vote_account_pubkeys: Vec<Pubkey>,
}

/// [`VoteMessage`] along with other information needed to sig verify it.
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
#[derive(Clone, Debug)]
pub(super) struct UnverifiedVotePayload {
    pub vote_message: UnverifiedVoteMessage,
    pub sender_bls_pubkey: PopVerified<BlsPubkeyAffine>,
    pub sender_vote_account_pubkey: Pubkey,
    pub sender_identity_pubkey: Pubkey,
    pub rank: u16,
    pub stake: NonZero<u64>,
}

impl UnverifiedVotePayload {
    fn verify(
        &self,
        max_validators: usize,
        prepared_hashed_message: &PreparedHashedMessage,
    ) -> Result<VerifiedVotePayload, BlsError> {
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
        Ok(VerifiedVotePayload {
            vote_aggregate,
            sender_vote_account_pubkeys: vec![self.sender_vote_account_pubkey],
        })
    }
}

/// Verifies votes and sends the verified votes to the consensus pool; and sends the desired subset
/// to rewards container and repair.
///
/// Any vote that fails fallback individual signature verification will have its sender banlisted.
pub(super) fn verify_and_send_votes(
    unverified_votes: &HashMap<
        VotePayloadToSign,
        (Vec<UnverifiedVotePayload>, Arc<BLSPubkeyToRankMap>),
    >,
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
            .par_iter()
            .fold(
                ParResult::default,
                |mut par_result, (vote_payload_to_sign, (unverified_votes, rank_map))| {
                    let num_votes_to_sigverify = unverified_votes.len();
                    let vote = Vote::from(*vote_payload_to_sign);
                    let max_validators = rank_map.len();
                    let (verified_votes, vote_verification_stats) = verify_votes(
                        max_validators,
                        vote_payload_to_sign,
                        unverified_votes,
                        ban_sender,
                        thread_pool,
                    );
                    par_result.add(
                        vote,
                        verified_votes,
                        vote_verification_stats,
                        num_votes_to_sigverify,
                    );
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
    verified_votes: Vec<(Vote, Vec<VerifiedVotePayload>)>,
) -> Result<VoteSenderStats, SigVerifyVoteError> {
    let mut sender_stats = VoteSenderStats::default();
    for (vote, payloads) in verified_votes {
        let (aggregates, pubkeys_grouped): (Vec<_>, Vec<_>) = payloads
            .into_iter()
            .map(|payload| (payload.vote_aggregate, payload.sender_vote_account_pubkeys))
            .unzip();
        let pubkeys = pubkeys_grouped.into_iter().flatten().collect::<Vec<_>>();
        if rewards_wants_vote(my_pubkey, leader_schedule, root_bank.slot(), &vote) {
            send_votes_to_rewards(
                my_pubkey,
                aggregates.clone(),
                &channels.channel_to_reward,
                &mut sender_stats,
            );
        }
        send_sig_verified_batch_to_pool(
            my_pubkey,
            SigVerifiedBatch::Votes(aggregates),
            &channels.channel_to_pool,
            &mut sender_stats,
        )?;
        match vote {
            Vote::Notarize(_) | Vote::Finalize(_) | Vote::NotarizeFallback(_) => {
                let vote_slot = vote.slot();
                let repair_msg = HashMap::from([(vote_slot, pubkeys.clone())]);
                send_votes_to_repair(
                    my_pubkey,
                    repair_msg,
                    &channels.channel_to_repair,
                    &mut sender_stats,
                );
            }
            Vote::Skip(_) | Vote::SkipFallback(_) | Vote::Genesis(_) => (),
        }
        let metrics_msg = ConsensusMetricsEvent::Vote { ids: pubkeys, vote };
        send_votes_to_metrics(
            my_pubkey,
            vec![metrics_msg],
            &channels.channel_to_metrics,
            &mut sender_stats,
        );
    }
    Ok(sender_stats)
}

/// Sig verifies `unverified_votes` and returns a `Vec` of votes that passed verification.
fn verify_votes(
    max_validators: usize,
    vote_payload_to_sign: &VotePayloadToSign,
    unverified_votes: &[UnverifiedVotePayload],
    ban_sender: &BanSender,
    thread_pool: &ThreadPool,
) -> (Vec<VerifiedVotePayload>, VoteVerificationStats) {
    let mut stats = VoteVerificationStats::default();

    // no need to do optimistic verification when batch size == 1.
    if let [unverified_vote] = unverified_votes {
        let ((verification_result, sender_identity_pubkey), time_us) = measure_us!({
            let serialized_vote = wincode::serialize(&vote_payload_to_sign).unwrap();
            let prepared_hash_msg = PreparedHashedMessage::new(&serialized_vote);
            let sender_identity_pubkey = unverified_vote.sender_identity_pubkey;
            (
                unverified_vote.verify(max_validators, &prepared_hash_msg),
                sender_identity_pubkey,
            )
        });
        stats.fn_verify_individual_votes_stats.add_sample(time_us);
        return match verification_result {
            Ok(verified_vote) => {
                stats.num_individual_verified += 1;
                (vec![verified_vote], stats)
            }
            Err(error) => {
                ban_invalid_vote_sender(ban_sender, &mut stats, sender_identity_pubkey, error);
                (Vec::new(), stats)
            }
        };
    }

    // Try optimistic verification - fast to verify, but cannot identify invalid votes
    let res = verify_votes_optimistic(
        vote_payload_to_sign,
        unverified_votes,
        &mut stats,
        thread_pool,
    );

    match res {
        Either::Left(signature) => {
            stats.optimistic_verification_succeeded += 1;
            stats
                .optimistic_batch
                .add_sample(unverified_votes.len() as u64);
            let vote_aggregate = VoteAggregate::new_from_verified_votes(
                max_validators,
                *vote_payload_to_sign,
                unverified_votes.iter().map(|v| (v.rank, v.stake)),
                signature,
            );
            let sender_vote_account_pubkeys = unverified_votes
                .iter()
                .map(|v| v.sender_vote_account_pubkey)
                .collect();
            (
                vec![VerifiedVotePayload {
                    vote_aggregate,
                    sender_vote_account_pubkeys,
                }],
                stats,
            )
        }
        Either::Right(prepared_hash_msg) => {
            // Fallback to individual verification
            stats.optimistic_verification_failed += 1;
            let ((verified_votes, invalid_remote_pubkeys), time_us) =
                measure_us!(verify_individual_votes(
                    max_validators,
                    unverified_votes,
                    prepared_hash_msg,
                    thread_pool
                ));
            stats.num_individual_verified += verified_votes.len() as u64;
            for (sender_identity_pubkey, error) in invalid_remote_pubkeys {
                ban_invalid_vote_sender(ban_sender, &mut stats, sender_identity_pubkey, error);
            }
            stats.fn_verify_individual_votes_stats.add_sample(time_us);
            (verified_votes, stats)
        }
    }
}

fn ban_invalid_vote_sender(
    ban_sender: &BanSender,
    stats: &mut VoteVerificationStats,
    sender_identity_pubkey: Pubkey,
    error: BlsError,
) {
    stats.banning_validator += 1;
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
    vote_payload_to_sign: &VotePayloadToSign,
    unverified_votes: &[UnverifiedVotePayload],
    stats: &mut VoteVerificationStats,
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

    let mut measure = Measure::start("verify_votes_optimistic");

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

    measure.stop();
    stats
        .fn_verify_votes_optimistic_stats
        .add_sample(measure.as_us());
    if verified {
        Either::Left(aggregate_signature)
    } else {
        Either::Right(prepared_hash_msg)
    }
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
fn aggregate_signatures(votes: &[UnverifiedVotePayload]) -> Result<SignatureProjective, BlsError> {
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
    vote_payload_to_sign: &VotePayloadToSign,
    votes: &[UnverifiedVotePayload],
) -> (
    PreparedHashedMessage,
    Result<PopVerified<PubkeyProjective>, BlsError>,
) {
    debug_assert!(current_thread_index().is_some());
    let serialized_vote = wincode::serialize(vote_payload_to_sign).unwrap();
    let prepared_hash_msg = PreparedHashedMessage::new(&serialized_vote);
    // converting aggregate pubkey to `PopVerified` is safe here
    // since the pubkeys are all PoP verified in the vote account
    let pubkey =
        PubkeyProjective::par_aggregate(votes.into_par_iter().map(|v| &v.sender_bls_pubkey))
            .map(|agg| unsafe { PopVerified::new_unchecked(*agg) });
    (prepared_hash_msg, pubkey)
}

/// Verifies votes individually on a thread pool.
///
/// Returns:
/// - `Vec<VotePayload>`: votes that passed verification.
/// - `Vec<Pubkey>`: senders' identity pubkeys for votes that failed verification.
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
fn verify_individual_votes(
    max_validators: usize,
    unverified_votes: &[UnverifiedVotePayload],
    prepared_hash_msg: PreparedHashedMessage,
    thread_pool: &ThreadPool,
) -> (Vec<VerifiedVotePayload>, Vec<(Pubkey, BlsError)>) {
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

#[derive(Default)]
struct ParResult {
    verified_votes: Vec<(Vote, Vec<VerifiedVotePayload>)>,
    verification_stats: VoteVerificationStats,
    num_votes_to_sigverify: Saturating<usize>,
}

impl ParResult {
    fn add(
        &mut self,
        vote: Vote,
        verified_votes: Vec<VerifiedVotePayload>,
        verification_stats: VoteVerificationStats,
        num_votes_to_sigverify: usize,
    ) {
        if !verified_votes.is_empty() {
            self.verified_votes.push((vote, verified_votes));
        }
        self.verification_stats.merge(verification_stats);
        self.num_votes_to_sigverify += num_votes_to_sigverify;
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
