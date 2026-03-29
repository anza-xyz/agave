//! Module for [`NotarEntry`] which is used to track observed notar votes for building a [`NotarRewardCertificate`].
//! The struct handles different validators voting for different block ids and ensures that a given validator does not vote for multiple block ids.

use {
    super::{AddVoteError, BuildSigBitmapError, partial_cert::PartialCert},
    crate::consensus_rewards::BuildRewardCertsRespError,
    agave_votor_messages::reward_certificate::NotarRewardCertificate,
    solana_bls_signatures::Signature as BLSSignature,
    solana_clock::Slot,
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    solana_runtime::epoch_stakes::BLSPubkeyToRankMap,
    std::collections::{HashMap, HashSet},
};

/// Struct to manage per slot state for notar votes used to build a [`NotarRewardCertificate`].
#[derive(Clone)]
pub(super) struct NotarEntry {
    /// Stores which validators have already voted.
    voted: HashSet<u16>,
    /// Different validators may vote for different block ids.
    /// This stores a [`PartialCert`] per block id observed.
    partials: HashMap<Hash, PartialCert>,
}

impl NotarEntry {
    /// Returns a new instance of [`NotarEntry`].
    pub(super) fn new(max_validators: usize) -> Self {
        Self {
            voted: HashSet::with_capacity(max_validators),
            // under normal operations, all validators should vote for a single block id, still allocate space for a few more to hopefully avoid allocations.
            partials: HashMap::with_capacity(5),
        }
    }

    /// Returns true if the [`NotarEntry`] needs the vote else false.
    pub(super) fn wants_vote(&self, rank: u16) -> bool {
        !self.voted.contains(&rank)
    }

    /// Adds a new observed vote to the aggregate.
    pub(super) fn add_vote(
        &mut self,
        rank_map: &BLSPubkeyToRankMap,
        rank: u16,
        signature: &BLSSignature,
        block_id: Hash,
        max_validators: usize,
    ) -> Result<(), AddVoteError> {
        if !self.voted.insert(rank) {
            return Err(AddVoteError::Duplicate);
        }
        let partial = self
            .partials
            .entry(block_id)
            .or_insert(PartialCert::new(max_validators));
        let res = partial.add_vote(rank_map, rank, signature);
        if res.is_err() {
            self.voted.remove(&rank);
        }
        res
    }

    /// Builds a [`NotarRewardCertificate`] and a list of validators in the certs from the observed votes.
    pub(super) fn build_cert(
        self,
        reward_slot: Slot,
    ) -> Result<Option<(NotarRewardCertificate, Vec<Pubkey>)>, BuildRewardCertsRespError> {
        // we can only submit one notar rewards certificate but different validators may vote for different blocks and we cannot combine notar votes for different blocks together in one cert.
        // ideally we should pick the block_id with the most stake to maximum leader rewards.
        // we expect this to be rare enough that picking the block_id with the most votes should be fine in most cases.

        let res = self
            .partials
            .into_iter()
            .max_by_key(|(_block_id, partial)| partial.votes_seen());
        let Some((block_id, partial)) = res else {
            return Ok(None);
        };
        match partial.build_sig_bitmap() {
            Err(e) => match e {
                BuildSigBitmapError::Empty => Ok(None),
                BuildSigBitmapError::Encode(e) => Err(BuildRewardCertsRespError::Encode(e)),
            },
            Ok((signature, bitmap, validators)) => {
                let cert =
                    NotarRewardCertificate::try_new(reward_slot, block_id, signature, bitmap)?;
                Ok(Some((cert, validators)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::consensus_rewards::entry::tests::{
            get_rank_map_keypairs, new_vote, validate_bitmap,
        },
        agave_votor_messages::{consensus_message::VoteMessage, vote::Vote},
        solana_hash::Hash,
    };

    #[test]
    fn validator_add_vote() {
        let slot = 123;
        let max_validators = 5;
        let (rank_map, keypairs) = get_rank_map_keypairs(max_validators, slot);
        let rank = 0;
        let mut entry = NotarEntry::new(max_validators);

        let blockid0 = Hash::new_unique();
        let notar = Vote::new_notarization_vote(slot, blockid0);
        let invalid_vote = VoteMessage {
            vote: notar,
            signature: BLSSignature::default(),
            rank,
        };
        entry
            .add_vote(
                &rank_map,
                invalid_vote.rank,
                &invalid_vote.signature,
                blockid0,
                max_validators,
            )
            .unwrap_err();

        let vote = new_vote(notar, rank as usize, &keypairs);
        entry
            .add_vote(
                &rank_map,
                vote.rank,
                &vote.signature,
                blockid0,
                max_validators,
            )
            .unwrap();
        let err = entry
            .add_vote(
                &rank_map,
                vote.rank,
                &vote.signature,
                blockid0,
                max_validators,
            )
            .unwrap_err();
        assert!(matches!(err, AddVoteError::Duplicate));
    }

    #[test]
    fn validate_build_cert() {
        let slot = 123;
        let max_validators = 5;
        let (rank_map, keypairs) = get_rank_map_keypairs(max_validators, slot);

        let mut entry = NotarEntry::new(max_validators);
        assert_eq!(entry.clone().build_cert(slot).unwrap(), None);

        let blockid0 = Hash::new_unique();
        let blockid1 = Hash::new_unique();

        for rank in 0..2 {
            let notar = Vote::new_notarization_vote(slot, blockid0);
            let vote = new_vote(notar, rank, &keypairs);
            entry
                .add_vote(
                    &rank_map,
                    vote.rank,
                    &vote.signature,
                    blockid0,
                    max_validators,
                )
                .unwrap();
        }
        for rank in 2..5 {
            let notar = Vote::new_notarization_vote(slot, blockid1);
            let vote = new_vote(notar, rank, &keypairs);
            entry
                .add_vote(
                    &rank_map,
                    vote.rank,
                    &vote.signature,
                    blockid1,
                    max_validators,
                )
                .unwrap();
        }
        let (notar_cert, _) = entry.build_cert(slot).unwrap().unwrap();
        assert_eq!(notar_cert.slot, slot);
        assert_eq!(notar_cert.block_id, blockid1);
        validate_bitmap(notar_cert.bitmap(), 3, 5);
    }
}
