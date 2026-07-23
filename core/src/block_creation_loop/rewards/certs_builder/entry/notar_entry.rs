//! Module for [`NotarEntry`] which is used to track observed notar votes for building a [`NotarRewardCertificate`].
//! The struct handles different validators voting for different block ids and ensures that a given validator does not vote for multiple block ids.

use {
    super::{AddVoteError, BuildSigBitmapError, partial_cert::PartialCert},
    agave_bls_sigverify::rewards::RewardVoteMessage,
    agave_votor_messages::reward_certificate::{BuildRewardCertsRespError, NotarRewardCertificate},
    solana_clock::Slot,
    solana_hash::Hash,
    solana_pubkey::Pubkey,
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
        msg: &RewardVoteMessage,
        block_id: Hash,
        max_validators: usize,
    ) -> Result<(), AddVoteError> {
        if !self.voted.insert(msg.rank) {
            return Err(AddVoteError::Duplicate);
        }
        let partial = self
            .partials
            .entry(block_id)
            .or_insert(PartialCert::new(max_validators));
        let res = partial.add_vote(msg);
        if res.is_err() {
            self.voted.remove(&msg.rank);
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
            get_keypair_with_stakes, get_keypairs, new_reward_vote_msg, validate_bitmap,
        },
        agave_bls_sigverify::rewards::RewardVote,
        agave_votor_messages::{
            consensus_message::Block,
            vote::{NotarizationVote, Vote},
        },
        rand::Rng,
        solana_bls_signatures::{Signature as BLSSignature, signature::BLS_SIGNATURE_AFFINE_SIZE},
        solana_hash::Hash,
        std::num::NonZero,
    };

    #[test]
    fn validator_add_vote() {
        let slot = 123;
        let max_validators = 5;
        let shred_version = rand::rng().random();
        let keypairs = get_keypairs(max_validators, slot);
        let rank = 0;
        let mut entry = NotarEntry::new(max_validators);

        let blockid0 = Hash::new_unique();
        let block = Block {
            slot,
            block_id: blockid0,
        };
        let notar_vote = Vote::new_notarization_vote(block);
        let notar_reward_vote = RewardVote::Notar(NotarizationVote {
            block: Block {
                slot,
                block_id: blockid0,
            },
        });
        let invalid_reward_vote_msg = RewardVoteMessage {
            vote: notar_reward_vote,
            signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
            rank,
            stake: NonZero::new(1234).unwrap(),
            vote_account_pubkey: Pubkey::new_unique(),
        };
        entry
            .add_vote(&invalid_reward_vote_msg, blockid0, max_validators)
            .unwrap_err();

        let reward_vote_msg =
            new_reward_vote_msg(notar_vote, rank as usize, &keypairs, None, shred_version);
        entry
            .add_vote(&reward_vote_msg, blockid0, max_validators)
            .unwrap();
        let err = entry
            .add_vote(&reward_vote_msg, blockid0, max_validators)
            .unwrap_err();
        assert!(matches!(err, AddVoteError::Duplicate));
    }

    #[test]
    fn validate_build_cert() {
        let slot = 123;
        let max_validators = 5;
        let stakes = vec![1_000, 900, 10, 10, 10];
        let keypairs = get_keypair_with_stakes(stakes.clone(), slot);
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
            let reward_vote_msg =
                new_reward_vote_msg(notar, rank, &keypairs, Some(&stakes), shred_version);
            entry
                .add_vote(&reward_vote_msg, blockid0, max_validators)
                .unwrap();
        }
        for rank in 2..5 {
            let notar = Vote::new_notarization_vote(Block {
                slot,
                block_id: blockid1,
            });
            let reward_vote_msg =
                new_reward_vote_msg(notar, rank, &keypairs, Some(&stakes), shred_version);
            entry
                .add_vote(&reward_vote_msg, blockid1, max_validators)
                .unwrap();
        }
        let (notar_cert, _) = entry.build_cert(slot).unwrap().unwrap();
        assert_eq!(notar_cert.slot, slot);
        // We should pick the block id with the most stake (not the most votes)
        assert_eq!(notar_cert.block_id, blockid0);
        validate_bitmap(notar_cert.bitmap(), 2, 5);
    }
}
