#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
#[cfg(debug_assertions)]
use std::collections::HashSet;
use {
    agave_votor_messages::{
        consensus_message::VoteMessage, sig_verified_messages::VoteAggregate,
        wire::VotePayloadToSign,
    },
    rayon::{
        ThreadPool, current_thread_index,
        iter::{Either, IntoParallelRefIterator, ParallelIterator},
    },
    solana_bls_signatures::{
        BlsError, PreparedHashedMessage, PubkeyProjective, Signature as BLSSignature,
        SignatureProjective,
        pubkey::{PopVerified, VerifySignature},
    },
    solana_pubkey::Pubkey,
    solana_runtime::epoch_stakes::{BLSPubkeyStakeEntry, BLSPubkeyToRankMap},
};

#[derive(Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct GroupIndex(usize);

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) struct VerifiedVotePayload {
    pub(crate) vote_aggregate: VoteAggregate,
    pub(crate) sender_vote_account_pubkeys: Vec<Pubkey>,
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
#[derive(Default)]
pub(crate) struct UnverifiedVoteGroup {
    sig_ranks: Vec<(BLSSignature, u16)>,
}

impl UnverifiedVoteGroup {
    pub(crate) fn len(&self) -> usize {
        self.sig_ranks.len()
    }

    pub(crate) fn debug_assert_unique(&self) {
        #[cfg(debug_assertions)]
        {
            let deduped = self
                .sig_ranks
                .iter()
                .map(|(_, r)| r)
                .copied()
                .collect::<HashSet<_>>();
            assert_eq!(deduped.len(), self.len());
        }
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn aggregate_signatures(&self) -> Result<SignatureProjective, BlsError> {
        debug_assert!(current_thread_index().is_some());
        let signatures = self.sig_ranks.par_iter().map(|(s, _)| s);
        // TODO(sam): Currently, `par_aggregate` performs full validation
        // (on-curve + subgroup check) for every signature. Since the subgroup
        // check is expensive, we can use an `unchecked` deserialization here
        // (performing only the cheap on-curve check) and rely on a single subgroup
        // check on the final aggregated signature. This should save more than 80%
        // of the time for signature aggregation.
        SignatureProjective::par_aggregate(signatures)
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn aggregate_pubkeys_by_payload(
        &self,
        rank_map: &BLSPubkeyToRankMap,
        vote_payload_to_sign: &VotePayloadToSign,
    ) -> (
        PreparedHashedMessage,
        Result<PopVerified<PubkeyProjective>, BlsError>,
    ) {
        debug_assert!(current_thread_index().is_some());
        let serialized_vote = wincode::serialize(vote_payload_to_sign).unwrap();
        let prepared_hash_msg = PreparedHashedMessage::new(&serialized_vote);
        // converting aggregate pubkey to `PopVerified` is safe here
        // since the pubkeys are all PoP verified in the vote account
        let pubkey = PubkeyProjective::par_aggregate(
            self.sig_ranks
                .par_iter()
                .map(|(_, rank)| &get_entry_for_rank(rank_map, *rank).bls_pubkey),
        )
        .map(|agg| unsafe { PopVerified::new_unchecked(*agg) });
        (prepared_hash_msg, pubkey)
    }

    pub(crate) fn to_verified_vote_payload(
        &self,
        rank_map: &BLSPubkeyToRankMap,
        vote_payload_to_sign: VotePayloadToSign,
        signature: SignatureProjective,
    ) -> VerifiedVotePayload {
        let max_validators = rank_map.len();
        let vote_aggregate = VoteAggregate::new_from_verified_votes(
            max_validators,
            vote_payload_to_sign,
            self.sig_ranks.iter().map(|(_, rank)| {
                let stake = get_entry_for_rank(rank_map, *rank).stake;
                (*rank, stake)
            }),
            signature,
        );
        let sender_vote_account_pubkeys = self
            .sig_ranks
            .iter()
            .map(|(_, rank)| get_entry_for_rank(rank_map, *rank).vote_account_pubkey)
            .collect();
        VerifiedVotePayload {
            vote_aggregate,
            sender_vote_account_pubkeys,
        }
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn verify_individual_votes(
        &self,
        rank_map: &BLSPubkeyToRankMap,
        vote_payload_to_sign: VotePayloadToSign,
        prepared_hash_msg: PreparedHashedMessage,
        thread_pool: &ThreadPool,
    ) -> (Vec<VerifiedVotePayload>, Vec<(Pubkey, BlsError)>) {
        thread_pool.install(|| {
            self.sig_ranks
                .par_iter()
                .partition_map(|(signature, rank)| {
                    let entry = get_entry_for_rank(rank_map, *rank);
                    match entry
                        .bls_pubkey
                        .verify_signature_prepared(signature, &prepared_hash_msg)
                    {
                        Ok(()) => {
                            let vote_message = VoteMessage {
                                vote: vote_payload_to_sign.into(),
                                signature: *signature,
                                rank: *rank,
                                stake: entry.stake,
                            };
                            Either::Left(VerifiedVotePayload {
                                vote_aggregate: VoteAggregate::new_from_verified_vote(
                                    rank_map.len(),
                                    vote_message,
                                ),
                                sender_vote_account_pubkeys: vec![entry.vote_account_pubkey],
                            })
                        }
                        Err(error) => Either::Right((entry.node_pubkey, error)),
                    }
                })
        })
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn push(&mut self, signature: BLSSignature, rank: u16) {
        self.sig_ranks.push((signature, rank))
    }

    fn clear(&mut self) {
        self.sig_ranks.clear();
    }
}

#[derive(Default)]
pub(crate) struct UnverifiedVoteGroupArena {
    groups: Vec<UnverifiedVoteGroup>,
    ptr: GroupIndex,
}

impl UnverifiedVoteGroupArena {
    pub(crate) fn alloc(&mut self) -> GroupIndex {
        if self.ptr.0 == self.groups.len() {
            self.groups.push(UnverifiedVoteGroup::default());
        }
        self.groups[self.ptr.0].clear();
        let ret = self.ptr;
        self.ptr.0 = self.ptr.0.saturating_add(1);
        ret
    }

    pub(crate) fn get_mut(&mut self, group_index: GroupIndex) -> &mut UnverifiedVoteGroup {
        &mut self.groups[group_index.0]
    }

    pub(crate) fn get(&self, group_index: GroupIndex) -> &UnverifiedVoteGroup {
        &self.groups[group_index.0]
    }

    pub(crate) fn reset(&mut self) {
        self.ptr = GroupIndex(0)
    }
}

fn get_entry_for_rank(rank_map: &BLSPubkeyToRankMap, rank: u16) -> &BLSPubkeyStakeEntry {
    rank_map
        .get_pubkey_stake_entry(usize::from(rank))
        .expect("vote group rank must exist in the epoch rank map")
}
