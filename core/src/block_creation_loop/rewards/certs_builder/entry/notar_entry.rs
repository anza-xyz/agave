//! Module for [`NotarEntry`] which is used to track observed notar votes for building a [`NotarRewardCertificate`].
//! The struct handles different validators voting for different block ids and ensures that a given validator does not vote for multiple block ids.

use {
    super::{AddVoteError, BuildSigBitmapError, partial_cert::PartialCert},
    agave_bls_sigverify::sig_verified_messages::SigVerifiedVoteBatch,
    agave_votor_messages::reward_certificate::{BuildRewardCertsRespError, NotarRewardCertificate},
    bitvec::vec::BitVec,
    solana_clock::Slot,
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    solana_runtime::epoch_stakes::BLSPubkeyToRankMap,
    std::collections::HashMap,
};

/// Struct to manage per slot state for notar votes used to build a [`NotarRewardCertificate`].
#[derive(Clone)]
pub(super) struct NotarEntry {
    /// Stores which validators have already voted.
    voted: BitVec<u8>,
    /// Different validators may vote for different block ids.
    /// This stores a [`PartialCert`] per block id observed.
    partials: HashMap<Hash, PartialCert>,
}

impl NotarEntry {
    /// Returns a new instance of [`NotarEntry`].
    pub(super) fn new(max_validators: usize) -> Self {
        Self {
            voted: BitVec::repeat(false, max_validators),
            // under normal operations, all validators should vote for a single block id, still allocate space for a few more to hopefully avoid allocations.
            partials: HashMap::with_capacity(5),
        }
    }

    /// Returns true if the [`NotarEntry`] needs the vote else false.
    pub(super) fn wants_vote(&self, ranks: &BitVec<u8>) -> bool {
        ranks
            .iter()
            .by_vals()
            .zip(self.voted.iter().by_vals())
            .any(|(x, y)| x && y)
    }

    /// Adds a new observed vote to the aggregate.
    pub(super) fn add_vote(
        &mut self,
        rank_map: &BLSPubkeyToRankMap,
        max_validators: usize,
        block_id: Hash,
        vote: &SigVerifiedVoteBatch,
    ) -> Result<(), AddVoteError> {
        let partial = self
            .partials
            .entry(block_id)
            .or_insert(PartialCert::new(max_validators));
        let res = partial.add_vote(rank_map, vote);
        if res.is_ok() {
            self.voted |= vote.ranks();
        }
        res
    }

    /// Builds a [`NotarRewardCertificate`] and a list of validators in the certs from the observed votes.
    pub(super) fn build_cert(
        self,
        reward_slot: Slot,
    ) -> Result<Option<(NotarRewardCertificate, Vec<Pubkey>)>, BuildRewardCertsRespError> {
        // We can only submit one notar rewards certificate, but different validators may vote for
        // different block ids. Pick the block id with the most stake to maximize leader rewards.
        let selected = self
            .partials
            .into_iter()
            .max_by_key(|(_block_id, partial)| partial.stake());
        let Some((block_id, partial)) = selected else {
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
        crate::block_creation_loop::rewards::certs_builder::entry::tests::{
            get_rank_map_keypairs, get_rank_map_keypairs_with_stakes, new_vote, validate_bitmap,
        },
        agave_votor_messages::{consensus_message::Block, vote::Vote},
        rand::Rng,
        solana_hash::Hash,
    };

    #[test]
    fn validator_add_vote() {
        let slot = 123;
        let max_validators = 5;
        let shred_version = rand::rng().random();
        let (rank_map, keypairs, bank, _forks) = get_rank_map_keypairs(max_validators, slot);
        let rank = 0;
        let mut entry = NotarEntry::new(max_validators);

        let blockid0 = Hash::new_unique();
        let notar = Vote::new_notarization_vote(Block {
            slot,
            block_id: blockid0,
        });

        let vote = new_vote(&bank, notar, rank, &keypairs, shred_version);
        entry
            .add_vote(&rank_map, max_validators, blockid0, &vote)
            .unwrap();
        let err = entry
            .add_vote(&rank_map, max_validators, blockid0, &vote)
            .unwrap_err();
        assert!(matches!(err, AddVoteError::Duplicate));
    }

    #[test]
    fn validate_build_cert() {
        let slot = 123;
        let max_validators = 5;
        let (rank_map, keypairs, bank, _bank_forks) =
            get_rank_map_keypairs_with_stakes(vec![1_000, 900, 10, 10, 10], slot);
        let shred_version = rand::rng().random();

        let mut entry = NotarEntry::new(max_validators);
        assert_eq!(entry.clone().build_cert(slot).unwrap(), None);

        let blockid0 = Hash::new_unique();
        let blockid1 = Hash::new_unique();

        for rank in 0..2 {
            let notar = Vote::new_notarization_vote(Block {
                slot,
                block_id: blockid0,
            });
            let vote = new_vote(&bank, notar, rank, &keypairs, shred_version);
            entry
                .add_vote(&rank_map, max_validators, blockid0, &vote)
                .unwrap();
        }
        for rank in 2..5 {
            let notar = Vote::new_notarization_vote(Block {
                slot,
                block_id: blockid1,
            });
            let vote = new_vote(&bank, notar, rank, &keypairs, shred_version);
            entry
                .add_vote(&rank_map, max_validators, blockid1, &vote)
                .unwrap();
        }
        let (notar_cert, _) = entry.build_cert(slot).unwrap().unwrap();
        assert_eq!(notar_cert.slot, slot);
        // We should pick the block id with the most stake (not the most votes)
        assert_eq!(notar_cert.block_id, blockid0);
        validate_bitmap(notar_cert.bitmap(), 2, 5);
    }
}
