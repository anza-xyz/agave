#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    crate::{
        UnverifiedVotesMessage,
        bls_sigverifier::BAN_TIMEOUT,
        bls_vote_sigverify::{UnverifiedVotePayload, VerifiedVotePayload},
        stats::VotesVerifierStats,
    },
    agave_votor_messages::{sig_verified_messages::VoteAggregate, wire::VotePayloadToSign},
    agave_votor_transport::endpoint::BanSender,
    crossbeam_channel::{Receiver, Sender},
    log::{error, info},
    rayon::{
        ThreadPool, current_thread_index,
        iter::{Either, IntoParallelIterator, IntoParallelRefIterator, ParallelIterator},
    },
    solana_bls_signatures::{
        BlsError, PreparedHashedMessage, PubkeyProjective, SignatureProjective, VerifySignature,
        pubkey::PopVerified,
    },
    solana_measure::{measure::Measure, measure_us},
    solana_pubkey::Pubkey,
    solana_runtime::{bank_forks::SharableBanks, epoch_stakes::BLSPubkeyToRankMap},
    std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    },
};

pub(crate) struct VotesVerifier {
    exit: Arc<AtomicBool>,
    ban_sender: BanSender,
    thread_pool: Arc<ThreadPool>,
    unverified_votes_receiver: Receiver<UnverifiedVotesMessage>,
    verified_votes_sender: Sender<Vec<Vec<VerifiedVotePayload>>>,
    stats: VotesVerifierStats,
    sharable_banks: SharableBanks,
}

impl VotesVerifier {
    pub(crate) fn new(
        exit: Arc<AtomicBool>,
        unverified_votes_receiver: Receiver<UnverifiedVotesMessage>,
        verified_votes_sender: Sender<Vec<Vec<VerifiedVotePayload>>>,
        ban_sender: BanSender,
        thread_pool: Arc<ThreadPool>,
        sharable_banks: SharableBanks,
    ) -> Self {
        let root_slot = sharable_banks.root().slot();
        Self {
            exit,
            unverified_votes_receiver,
            verified_votes_sender,
            ban_sender,
            thread_pool,
            sharable_banks,
            stats: VotesVerifierStats::new(root_slot),
        }
    }

    fn recv(&self) -> Result<UnverifiedVotesMessage, ()> {
        self.unverified_votes_receiver.recv().map_err(|_| ())
    }

    fn send(&self, votes: Vec<Vec<VerifiedVotePayload>>) -> Result<(), ()> {
        self.verified_votes_sender.send(votes).map_err(|_| ())
    }

    fn verify_vote_batch(
        &self,
        vote_payload_to_sign: VotePayloadToSign,
        unverified_votes: Vec<UnverifiedVotePayload>,
        rank_map: Arc<BLSPubkeyToRankMap>,
    ) -> (Vec<VerifiedVotePayload>, usize) {
        let max_validators = rank_map.len();
        verify_votes(
            max_validators,
            vote_payload_to_sign,
            unverified_votes,
            &self.ban_sender,
            &self.thread_pool,
        )
    }

    fn verify_and_send_votes(
        &mut self,
        votes_map: HashMap<
            VotePayloadToSign,
            (Vec<UnverifiedVotePayload>, Arc<BLSPubkeyToRankMap>),
        >,
    ) -> Result<(), ()> {
        let len = votes_map.len();
        let (verified, total_votes) = self.thread_pool.install(|| {
            votes_map
                .into_par_iter()
                .fold(
                    || (Vec::with_capacity(len), 0usize),
                    |(mut acc_votes, acc_cnt), (vote_payload_to_sign, (votes, rank_map))| {
                        let (verified_votes, num_votes_verified) =
                            self.verify_vote_batch(vote_payload_to_sign, votes, rank_map);
                        acc_votes.push(verified_votes);
                        (acc_votes, acc_cnt.saturating_add(num_votes_verified))
                    },
                )
                .reduce(
                    || (Vec::with_capacity(len), 0),
                    |mut left, mut right| {
                        left.0.append(&mut right.0);
                        (left.0, left.1.saturating_add(right.1))
                    },
                )
        });
        self.stats.votes_verified += total_votes as u64;
        self.send(verified).map_err(|_| ())
    }

    pub(crate) fn run(mut self) {
        while !self.exit.load(Ordering::Relaxed) {
            let Ok(votes_map) = self.recv() else {
                error!("votes receiver channel disconnected.  Exiting.");
                break;
            };
            if let Err(()) = self.verify_and_send_votes(votes_map) {
                error!("verified votes sender channel disconnected.  Exiting.");
                break;
            }
            let root_slot = self.sharable_banks.root().slot();
            self.stats.maybe_report(root_slot);
        }
    }
}

/// Sig verifies `unverified_votes` and returns a `Vec` of votes that passed verification.
fn verify_votes(
    max_validators: usize,
    vote_payload_to_sign: VotePayloadToSign,
    unverified_votes: Vec<UnverifiedVotePayload>,
    ban_sender: &BanSender,
    thread_pool: &ThreadPool,
) -> (Vec<VerifiedVotePayload>, usize) {
    // no need to do optimistic verification when batch size == 1.
    if let [unverified_vote] = unverified_votes.as_slice() {
        let ((verification_result, sender_identity_pubkey), _time_us) = measure_us!({
            let serialized_vote = wincode::serialize(&vote_payload_to_sign).unwrap();
            let prepared_hash_msg = PreparedHashedMessage::new(&serialized_vote);
            let sender_identity_pubkey = unverified_vote.sender_identity_pubkey;
            (
                unverified_vote.verify(max_validators, &prepared_hash_msg),
                sender_identity_pubkey,
            )
        });
        return match verification_result {
            Ok(verified_vote) => (vec![verified_vote], 1),
            Err(error) => {
                ban_invalid_vote_sender(ban_sender, sender_identity_pubkey, error);
                (Vec::new(), 0)
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
            let num_votes_verified = unverified_votes.len();
            let sender_vote_account_pubkeys = unverified_votes
                .into_iter()
                .map(|v| v.sender_vote_account_pubkey)
                .collect();
            (
                vec![VerifiedVotePayload {
                    vote_aggregate,
                    sender_vote_account_pubkeys,
                }],
                num_votes_verified,
            )
        }
        Either::Right(prepared_hash_msg) => {
            // Fallback to individual verification
            let ((verified_votes, invalid_remote_pubkeys), _time_us) =
                measure_us!(verify_individual_votes(
                    max_validators,
                    unverified_votes,
                    prepared_hash_msg,
                    thread_pool
                ));
            for (sender_identity_pubkey, error) in invalid_remote_pubkeys {
                ban_invalid_vote_sender(ban_sender, sender_identity_pubkey, error);
            }
            let num_votes_verified = verified_votes.len();
            (verified_votes, num_votes_verified)
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
    unverified_votes: &[UnverifiedVotePayload],
    thread_pool: &ThreadPool,
) -> Either<SignatureProjective, PreparedHashedMessage> {
    #[cfg(debug_assertions)]
    {
        use std::collections::HashSet;

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
    vote_payload_to_sign: VotePayloadToSign,
    votes: &[UnverifiedVotePayload],
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

/// Verifies votes individually on a thread pool.
///
/// Returns:
/// - `Vec<VotePayload>`: votes that passed verification.
/// - `Vec<Pubkey>`: senders' identity pubkeys for votes that failed verification.
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
fn verify_individual_votes(
    max_validators: usize,
    unverified_votes: Vec<UnverifiedVotePayload>,
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
