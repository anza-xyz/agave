use {
    crate::{
        bls_sigverifier::BAN_TIMEOUT,
        sig_verified_messages::VoteAggregate,
        vote_pool::{UnverifiedVotePayload, VerifiedVotePayload, VotePool},
    },
    agave_votor_messages::wire::VotePayloadToSign,
    log::info,
    rayon::{
        ThreadPool, current_thread_index,
        iter::{IntoParallelIterator, IntoParallelRefIterator, ParallelIterator},
    },
    solana_bls_signatures::{
        BlsError, PreparedHashedMessage, PubkeyProjective, SignatureProjective, VerifySignature,
        pubkey::PopVerified,
    },
    solana_measure::{measure::Measure, measure_us},
    solana_runtime::bank::Bank,
    std::sync::Arc,
};

impl VotePool {
    pub(super) fn verify_votes(
        &self,
        root_bank: &Bank,
        vote_payload_to_sign: VotePayloadToSign,
        mut unverified_votes: Vec<UnverifiedVotePayload>,
        thread_pool: &ThreadPool,
    ) -> Vec<VerifiedVotePayload> {
        let optimistic_result =
            self.verify_votes_optimistic(vote_payload_to_sign, &mut unverified_votes, thread_pool);
        if let Some(aggregate_signature) = optimistic_result {
            let Some(vote_aggregate) = VoteAggregate::new_from_verified_votes(
                root_bank,
                vote_payload_to_sign,
                unverified_votes.iter().map(|v| v.vote_message.rank),
                aggregate_signature,
            ) else {
                return vec![];
            };
            return vec![VerifiedVotePayload {
                vote_aggregate,
                sender_vote_account_pubkeys: unverified_votes
                    .into_iter()
                    .map(|v| v.sender_vote_account_pubkey)
                    .collect(),
            }];
        }

        // Fallback to individual verification
        let ((verified_votes, invalid_remote_pubkeys), _time_us) =
            measure_us!(self.verify_individual_votes(root_bank, unverified_votes, thread_pool));
        for sender_identity_pubkey in invalid_remote_pubkeys {
            if self.banlist.ban(sender_identity_pubkey, BAN_TIMEOUT) {
                // TODO: fix
                // stats.already_banned += 1;
            } else {
                info!(
                    "bls_vote_sigverify: banned sender={sender_identity_pubkey} due to failed \
                     verification"
                );
            }
        }
        // TODO: fix
        // stats.fn_verify_individual_votes_stats.add_sample(time_us);

        verified_votes
    }

    #[must_use]
    fn verify_votes_optimistic(
        &self,
        vote_payload_to_sign: VotePayloadToSign,
        unverified_votes: &mut Vec<UnverifiedVotePayload>,
        thread_pool: &ThreadPool,
    ) -> Option<SignatureProjective> {
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
            return None;
        };

        let Ok(aggregate_pubkey) = pubkey_result else {
            return None;
        };

        let verified = aggregate_pubkey
            .verify_signature_prepared(&aggregate_signature, &prepared_hash_msg)
            .is_ok();

        measure.stop();
        // TODO: fix
        // stats
        //     .fn_verify_votes_optimistic_stats
        //     .add_sample(measure.as_us());
        if verified {
            Some(aggregate_signature)
        } else {
            let prepared_hash_msg = Arc::new(prepared_hash_msg);
            for unverified_vote in unverified_votes {
                unverified_vote.prepared_payload = Some(prepared_hash_msg.clone());
            }
            None
        }
    }
}

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
