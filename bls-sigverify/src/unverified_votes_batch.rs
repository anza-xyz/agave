#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
#[cfg(debug_assertions)]
use std::collections::HashSet;
use {
    crate::{
        bls_sigverifier::BAN_TIMEOUT, stats::VoteVerificationStats, verified_batch::VerifiedBatch,
    },
    agave_votor_messages::{
        consensus_message::VoteMessage, sig_verified_messages::VoteAggregate,
        unverified_vote_message::UnverifiedVoteMessage, vote::Vote, wire::VotePayloadToSign,
    },
    agave_votor_transport::endpoint::BanSender,
    log::info,
    rayon::{
        ThreadPool, current_thread_index,
        iter::{
            Either, IndexedParallelIterator, IntoParallelIterator, IntoParallelRefIterator,
            ParallelIterator,
        },
    },
    solana_bls_signatures::{
        BlsError, HashedMessage, PreparedHashedMessage, PubkeyProjective, SignatureProjective,
        VerifySignature,
        pubkey::{PopVerified, PubkeyAffine},
        signature::SignatureAffine,
    },
    solana_measure::{measure::Measure, measure_us},
    solana_pubkey::Pubkey,
    solana_runtime::epoch_stakes::BLSPubkeyToRankMap,
    std::{num::NonZero, sync::Arc},
};

/// A batch of votes to verify.
///
/// Length of `batch` should be same length of `sender_vote_account_pubkeys`.
pub(crate) struct UnverifiedBatch {
    vote_payload_to_sign: VotePayloadToSign,
    batch: Vec<UnverifiedVotePayload>,
    sender_vote_account_pubkeys: Vec<Pubkey>,
    rank_map: Arc<BLSPubkeyToRankMap>,
}

impl UnverifiedBatch {
    pub(crate) fn new(
        vote_payload_to_sign: VotePayloadToSign,
        payload: UnverifiedVotePayload,
        sender_vote_account_pubkey: Pubkey,
        rank_map: Arc<BLSPubkeyToRankMap>,
    ) -> Self {
        Self {
            vote_payload_to_sign,
            batch: vec![payload],
            sender_vote_account_pubkeys: vec![sender_vote_account_pubkey],
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
        self.batch.push(payload);
        self.sender_vote_account_pubkeys
            .push(sender_vote_account_pubkey);
    }

    pub(crate) fn len(&self) -> usize {
        self.batch.len()
    }

    pub(crate) fn verify(
        &mut self,
        ban_sender: &BanSender,
        thread_pool: &ThreadPool,
    ) -> (Option<VerifiedBatch>, VoteVerificationStats) {
        let mut stats = VoteVerificationStats::default();

        // no need to do optimistic verification when batch size == 1.
        if let [unverified_vote] = self.batch.as_slice() {
            let ((verification_result, sender_identity_pubkey), time_us) = measure_us!({
                let serialized_vote = wincode::serialize(&self.vote_payload_to_sign).unwrap();
                let sender_identity_pubkey = unverified_vote.sender_identity_pubkey;
                (
                    unverified_vote.verify(self.rank_map.len(), Either::Left(&serialized_vote)),
                    sender_identity_pubkey,
                )
            });
            stats.fn_verify_individual_votes_stats.add_sample(time_us);
            return match verification_result {
                Ok(vote_aggregate) => {
                    stats.num_individual_verified += 1;
                    let pubkeys = std::mem::take(&mut self.sender_vote_account_pubkeys);
                    (
                        Some(VerifiedBatch::new(
                            Vote::from(self.vote_payload_to_sign),
                            vec![vote_aggregate],
                            pubkeys,
                        )),
                        stats,
                    )
                }
                Err(error) => {
                    ban_invalid_vote_sender(ban_sender, &mut stats, sender_identity_pubkey, error);
                    (None, stats)
                }
            };
        }

        // Try optimistic verification - fast to verify, but cannot identify invalid votes
        let res = verify_votes_optimistic(
            &self.vote_payload_to_sign,
            &self.batch,
            &mut stats,
            thread_pool,
        );

        let sender_vote_account_pubkeys = std::mem::take(&mut self.sender_vote_account_pubkeys);
        match res {
            Ok(signature) => {
                stats.optimistic_verification_succeeded += 1;
                stats.optimistic_batch.add_sample(self.batch.len() as u64);
                let vote_aggregate = VoteAggregate::new_from_verified_votes(
                    self.rank_map.len(),
                    self.vote_payload_to_sign,
                    self.batch.iter().map(|v| (v.rank, v.stake)),
                    signature,
                );
                (
                    Some(VerifiedBatch::new(
                        Vote::from(self.vote_payload_to_sign),
                        vec![vote_aggregate],
                        sender_vote_account_pubkeys,
                    )),
                    stats,
                )
            }
            Err(hashed_msg) => {
                // Fallback to individual verification
                stats.optimistic_verification_failed += 1;
                let ((verified_batch, invalid_remote_pubkeys), time_us) =
                    measure_us!(verify_individual_votes(
                        Vote::from(self.vote_payload_to_sign),
                        self.rank_map.len(),
                        &self.batch,
                        sender_vote_account_pubkeys,
                        &hashed_msg,
                        thread_pool
                    ));
                if let Some(b) = &verified_batch {
                    stats.num_individual_verified += b.len();
                }
                for (sender_identity_pubkey, error) in invalid_remote_pubkeys {
                    ban_invalid_vote_sender(ban_sender, &mut stats, sender_identity_pubkey, error);
                }
                stats.fn_verify_individual_votes_stats.add_sample(time_us);
                (verified_batch, stats)
            }
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
// Return the prepared message on failure so fallback verification can reuse it
// without another hash-to-curve operation. Boxing it would add an allocation to
// this latency-sensitive path.
#[allow(clippy::result_large_err)]
fn verify_votes_optimistic(
    vote_payload_to_sign: &VotePayloadToSign,
    unverified_votes: &[UnverifiedVotePayload],
    stats: &mut VoteVerificationStats,
    thread_pool: &ThreadPool,
) -> Result<SignatureProjective, HashedMessage> {
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
    let (signature_result, (pubkey_result, hashed_msg)) = thread_pool.join(
        || aggregate_signatures(unverified_votes),
        || {
            thread_pool.join(
                || aggregate_pubkeys_by_payload(unverified_votes),
                || vote_payload_to_sign.to_hashed_msg(),
            )
        },
    );

    let Ok(aggregate_signature) = signature_result else {
        return Err(hashed_msg);
    };

    let Ok(aggregate_pubkey) = pubkey_result else {
        return Err(hashed_msg);
    };

    let verified = aggregate_pubkey.verify_signature_pre_hashed(&aggregate_signature, &hashed_msg);

    measure.stop();
    stats
        .fn_verify_votes_optimistic_stats
        .add_sample(measure.as_us());
    match verified {
        Ok(()) => Ok(aggregate_signature),
        Err(_) => Err(hashed_msg),
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
    votes: &[UnverifiedVotePayload],
) -> Result<PopVerified<PubkeyProjective>, BlsError> {
    debug_assert!(current_thread_index().is_some());
    // converting aggregate pubkey to `PopVerified` is safe here
    // since the pubkeys are all PoP verified in the vote account
    PubkeyProjective::par_aggregate(votes.into_par_iter().map(|v| &v.sender_bls_pubkey))
        .map(|agg| unsafe { PopVerified::new_unchecked(*agg) })
}

/// Verifies votes individually on a thread pool.
///
/// Returns:
/// - `Vec<VotePayload>`: votes that passed verification.
/// - `Vec<Pubkey>`: senders' identity pubkeys for votes that failed verification.
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
fn verify_individual_votes(
    vote: Vote,
    max_validators: usize,
    unverified_votes: &[UnverifiedVotePayload],
    sender_vote_account_pubkeys: Vec<Pubkey>,
    hashed_msg: &HashedMessage,
    thread_pool: &ThreadPool,
) -> (Option<VerifiedBatch>, Vec<(Pubkey, BlsError)>) {
    let prepared_msg = PreparedHashedMessage::from_hashed_message(hashed_msg);
    let (aggregates, sender_vote_account_pubkeys, failed) = thread_pool.install(|| {
        unverified_votes
            .into_par_iter()
            .zip(sender_vote_account_pubkeys)
            .fold(
                || (vec![], vec![], vec![]),
                |(mut verified, mut sender_vote_account_pubkeys, mut failed),
                 (unverified_vote, sender_vote_account_pubkey)| {
                    let sender_identity_pubkey = unverified_vote.sender_identity_pubkey;
                    match unverified_vote.verify(max_validators, Either::Right(&prepared_msg)) {
                        Ok(aggregate) => {
                            verified.push(aggregate);
                            sender_vote_account_pubkeys.push(sender_vote_account_pubkey);
                        }
                        Err(e) => failed.push((sender_identity_pubkey, e)),
                    }
                    (verified, sender_vote_account_pubkeys, failed)
                },
            )
            .reduce(
                || (vec![], vec![], vec![]),
                |mut left, mut right| {
                    left.0.append(&mut right.0);
                    left.1.append(&mut right.1);
                    left.2.append(&mut right.2);
                    left
                },
            )
    });
    (
        (!aggregates.is_empty())
            .then(|| VerifiedBatch::new(vote, aggregates, sender_vote_account_pubkeys)),
        failed,
    )
}

/// [`VoteMessage`] along with other information needed to sig verify it.
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
#[derive(Clone, Debug)]
pub(super) struct UnverifiedVotePayload {
    pub vote_message: UnverifiedVoteMessage,
    pub sender_bls_pubkey: PopVerified<PubkeyAffine>,
    pub sender_identity_pubkey: Pubkey,
    pub rank: u16,
    pub stake: NonZero<u64>,
}

impl UnverifiedVotePayload {
    fn verify(
        &self,
        max_validators: usize,
        msg: Either<&[u8], &PreparedHashedMessage>,
    ) -> Result<VoteAggregate, BlsError> {
        let signature = SignatureAffine::try_from(self.vote_message.signature)?;
        match msg {
            Either::Left(bytes) => self.sender_bls_pubkey.verify_signature(&signature, bytes),
            Either::Right(prepared) => self
                .sender_bls_pubkey
                .verify_signature_prepared(&signature, prepared),
        }?;
        let vote_msg = VoteMessage {
            vote: self.vote_message.vote,
            signature,
            rank: self.rank,
            stake: self.stake,
        };
        Ok(VoteAggregate::new_from_verified_vote(
            max_validators,
            vote_msg,
        ))
    }
}
