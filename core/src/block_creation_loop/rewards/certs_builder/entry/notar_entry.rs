//! Module for [`NotarEntry`] which is used to track observed notar votes for building a [`NotarRewardCertificate`].

use {
    crate::block_creation_loop::rewards::certs_builder::entry::{
        AddAggregateError, BuildSigBitmapError, PartialCert,
    },
    agave_votor_messages::{
        consensus_message::VoteMessage,
        reward_certificate::{BuildRewardCertsRespError, NotarRewardCertificate},
        sig_verified_messages::VoteAggregate,
    },
    solana_clock::Slot,
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    std::collections::HashMap,
};

#[derive(Clone)]
/// Struct to manage per slot state for notar votes used to build a [`NotarRewardCertificate`].
pub(super) struct NotarEntry {
    /// Different validators may vote for different block ids.
    /// This stores a [`PartialCert`] per block id observed.
    partials: HashMap<Hash, PartialCert>,
}

impl NotarEntry {
    /// Returns a new instance of [`NotarEntry`].
    pub(super) fn new() -> Self {
        Self {
            // under normal operations, all validators should vote for a single block id, still allocate space for a few more to hopefully avoid allocations.
            partials: HashMap::with_capacity(5),
        }
    }

    /// Accumulates a new observed vote aggregate from another validator.
    pub(super) fn add_aggregate(
        &mut self,
        aggregate: VoteAggregate,
        vote_account_pubkeys: Vec<Pubkey>,
        block_id: Hash,
        max_validators: usize,
    ) -> Result<(), AddAggregateError> {
        let partial = self
            .partials
            .entry(block_id)
            .or_insert_with(|| PartialCert::new(max_validators));
        partial.add_aggregate(aggregate, vote_account_pubkeys)
    }

    /// Accumulates a new observed own vote msg.
    pub(super) fn add_own_msg(
        &mut self,
        vote_msg: VoteMessage,
        vote_account_pubkey: Pubkey,
        block_id: Hash,
        max_validators: usize,
    ) -> Result<(), AddAggregateError> {
        let partial = self
            .partials
            .entry(block_id)
            .or_insert_with(|| PartialCert::new(max_validators));
        partial.add_own_msg(vote_msg, vote_account_pubkey)
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
                BuildSigBitmapError::Accumulating(e) => {
                    Err(BuildRewardCertsRespError::RewardCertTryNew(e.into()))
                }
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
            get_keypair_with_stakes, get_keypairs, new_reward_vote_aggregate, validate_bitmap,
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
        let keypairs = get_keypairs(max_validators, slot);
        let rank = 0;
        let mut entry = NotarEntry::new();

        let blockid0 = Hash::new_unique();
        let block = Block {
            slot,
            block_id: blockid0,
        };
        let notar_vote = Vote::new_notarization_vote(block);

        let (aggregate, vote_account_pubkeys) =
            new_reward_vote_aggregate(notar_vote, rank as usize, &keypairs, None, shred_version);
        entry
            .add_aggregate(aggregate, vote_account_pubkeys, blockid0, max_validators)
            .unwrap();
    }

    #[test]
    fn validate_build_cert() {
        let slot = 123;
        let max_validators = 5;
        let stakes = vec![1_000, 900, 10, 10, 10];
        let keypairs = get_keypair_with_stakes(stakes.clone(), slot);
        let shred_version = rand::rng().random();

        let mut entry = NotarEntry::new();
        assert_eq!(entry.clone().build_cert(slot).unwrap(), None);

        let blockid0 = Hash::new_unique();
        let blockid1 = Hash::new_unique();

        for rank in 0..2 {
            let notar = Vote::new_notarization_vote(Block {
                slot,
                block_id: blockid0,
            });
            let (aggregate, vote_account_pubkeys) =
                new_reward_vote_aggregate(notar, rank, &keypairs, Some(&stakes), shred_version);
            entry
                .add_aggregate(aggregate, vote_account_pubkeys, blockid0, max_validators)
                .unwrap();
        }
        for rank in 2..5 {
            let notar = Vote::new_notarization_vote(Block {
                slot,
                block_id: blockid1,
            });
            let (aggregate, vote_account_pubkeys) =
                new_reward_vote_aggregate(notar, rank, &keypairs, Some(&stakes), shred_version);
            entry
                .add_aggregate(aggregate, vote_account_pubkeys, blockid1, max_validators)
                .unwrap();
        }
        let (notar_cert, _) = entry.build_cert(slot).unwrap().unwrap();
        assert_eq!(notar_cert.slot, slot);
        // We should pick the block id with the most stake (not the most votes)
        assert_eq!(notar_cert.block_id, blockid0);
        validate_bitmap(notar_cert.bitmap(), 2, 5);
    }
}
