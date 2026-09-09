use {
    super::BuildRewardCertsRespError,
    crate::block_creation_loop::rewards::{
        certs_builder::entry::partial_cert::BuildResult, msg_types::RewardRespSucc,
    },
    agave_votor::aggregate_accumulator::AggregateAccumulatorError,
    agave_votor_messages::{
        consensus_message::VoteMessage, reward_certificate::SkipRewardCertificate,
        sig_verified_messages::VoteAggregate, vote::Vote,
    },
    notar_entry::NotarEntry,
    partial_cert::PartialCert,
    solana_clock::Slot,
    solana_pubkey::Pubkey,
};

mod notar_entry;
mod partial_cert;

#[derive(Clone)]
/// Per slot container for storing notar and skip votes for creating rewards certificates.
pub(super) struct Entry {
    /// [`PartialCert`] for observed skip votes.
    skip: PartialCert,
    /// Struct to store state for observed notar votes.
    notar: NotarEntry,
    /// Maximum number of validators for the slot this entry is working on.
    max_validators: usize,
}

impl Entry {
    /// Creates a new instance of [`Entry`].
    pub(super) fn new(max_validators: usize) -> Self {
        Self {
            skip: PartialCert::new(max_validators),
            notar: NotarEntry::new(),
            max_validators,
        }
    }

    /// Adds the given [`VoteAggregate`] from another validator to the aggregate.
    pub(super) fn add_aggregate(
        &mut self,
        aggregate: VoteAggregate,
        vote_account_pubkeys: Vec<Pubkey>,
    ) -> Result<(), AggregateAccumulatorError> {
        match *aggregate.vote() {
            Vote::Notarize(notar) => self.notar.add_aggregate(
                aggregate,
                vote_account_pubkeys,
                notar.block.block_id,
                self.max_validators,
            ),
            Vote::Skip(_) => self.skip.add_aggregate(aggregate, vote_account_pubkeys),
            _ => Ok(()),
        }
    }

    /// Adds the given [`VoteMessage`] from this node itself to the aggregate.
    pub(super) fn add_own_msg(
        &mut self,
        vote_msg: VoteMessage,
        vote_account_pubkey: Pubkey,
    ) -> Result<(), AggregateAccumulatorError> {
        match vote_msg.vote {
            Vote::Notarize(notar) => self.notar.add_own_msg(
                vote_msg,
                vote_account_pubkey,
                notar.block.block_id,
                self.max_validators,
            ),
            Vote::Skip(_) => self.skip.add_own_msg(vote_msg, vote_account_pubkey),
            _ => Ok(()),
        }
    }

    /// Builds reward certificates from the observed votes.
    pub(super) fn build_certs(
        self,
        reward_slot: Slot,
    ) -> Result<RewardRespSucc, BuildRewardCertsRespError> {
        let notar = self.notar.build_cert(reward_slot)?;
        let skip = match self.skip.build_sig_bitmap() {
            BuildResult::Empty | BuildResult::Identity => None,
            BuildResult::EncodingError(e) => return Err(BuildRewardCertsRespError::Encoding(e)),
            BuildResult::Success {
                signature,
                bitmap,
                validators,
            } => {
                let cert = SkipRewardCertificate::try_new(reward_slot, signature, bitmap)?;
                Some((cert, validators))
            }
        };

        let (skip, notar, validators) = match (skip, notar) {
            (None, None) => (None, None, vec![]),
            (Some((skip_cert, skip_validators)), None) => (Some(skip_cert), None, skip_validators),
            (None, Some((notar_cert, notar_validators))) => {
                (None, Some(notar_cert), notar_validators)
            }
            (Some((skip_cert, skip_validators)), Some((notar_cert, notar_validators))) => {
                let mut validators = skip_validators;
                validators.extend(notar_validators);
                (Some(skip_cert), Some(notar_cert), validators)
            }
        };

        Ok(RewardRespSucc {
            skip,
            notar,
            validators,
        })
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        agave_votor_messages::{
            consensus_message::Block,
            vote::Vote,
            wire::{VotePayloadToSign, get_vote_payload_to_sign},
        },
        rand::Rng,
        solana_bls_signatures::{
            Keypair as BlsKeypair, PubkeyCompressed as BlsPubkeyCompressed, SignatureProjective,
        },
        solana_epoch_schedule::EpochSchedule,
        solana_hash::Hash,
        solana_pubkey::Pubkey,
        solana_runtime::{
            bank::{Bank, SlotLeader},
            genesis_utils::{ValidatorVoteKeypairs, create_genesis_config_with_vote_accounts},
        },
        solana_signer_store::{Decoded, decode},
        std::{collections::HashMap, num::NonZero},
    };

    pub(crate) fn validate_bitmap(bitmap: &[u8], num_set: usize, max_len: usize) {
        let bitvec = decode(bitmap, max_len).unwrap();
        match bitvec {
            Decoded::Base2(bitvec) => assert_eq!(bitvec.count_ones(), num_set),
            Decoded::Base3(_, _) => panic!("unexpected variant"),
        }
    }

    pub(crate) fn new_reward_vote_aggregate(
        vote: Vote,
        rank: usize,
        keypairs: &[BlsKeypair],
        stakes: Option<&[u64]>,
        shred_version: u16,
    ) -> (VoteAggregate, Vec<Pubkey>) {
        let serialized = get_vote_payload_to_sign(vote, shred_version);
        let signature = keypairs[rank].sign(&serialized).into();
        let stake = match stakes {
            None => NonZero::new(123).unwrap(),
            Some(stakes) => NonZero::new(stakes[rank]).unwrap(),
        };
        let vote_msg = VoteMessage {
            vote,
            signature,
            rank: rank.try_into().unwrap(),
            stake,
        };
        let aggregate = VoteAggregate::new_from_verified_vote(keypairs.len(), vote_msg);
        (aggregate, vec![Pubkey::new_unique()])
    }

    pub(crate) fn new_identity_reward_vote_aggregate(
        vote: Vote,
        max_validators: usize,
        ranks_and_stakes: &[(u16, u64)],
        shred_version: u16,
    ) -> (VoteAggregate, Vec<Pubkey>) {
        // This models the aggregate produced by individually valid votes whose signatures cancel.
        let aggregate = VoteAggregate::new_from_verified_votes(
            max_validators,
            VotePayloadToSign::new_from_vote(vote, shred_version),
            ranks_and_stakes.iter().map(|(rank, stake)| {
                (
                    *rank,
                    NonZero::new(*stake).expect("test stake must be nonzero"),
                )
            }),
            SignatureProjective::identity(),
        );
        let validators = ranks_and_stakes
            .iter()
            .map(|_| Pubkey::new_unique())
            .collect();
        (aggregate, validators)
    }

    pub(crate) fn get_keypairs(max_validators: usize, slot: Slot) -> Vec<BlsKeypair> {
        get_keypair_with_stakes(vec![100; max_validators], slot)
    }

    pub(crate) fn get_keypair_with_stakes(stakes: Vec<u64>, slot: Slot) -> Vec<BlsKeypair> {
        let max_validators = stakes.len();
        let validator_keypairs = (0..max_validators)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        let keypair_map = validator_keypairs
            .iter()
            .map(|k| {
                (
                    BlsPubkeyCompressed::from(k.bls_keypair.public.into_inner()),
                    k.bls_keypair.clone(),
                )
            })
            .collect::<HashMap<_, _>>();
        let mut genesis_config =
            create_genesis_config_with_vote_accounts(1_000_000_000, &validator_keypairs, stakes)
                .genesis_config;
        genesis_config.epoch_schedule = EpochSchedule::without_warmup();
        let (bank, bank_forks) =
            Bank::new_for_tests(&genesis_config).wrap_with_bank_forks_for_tests();
        let bank = Bank::new_from_parent_with_bank_forks(
            bank_forks.as_ref(),
            bank,
            SlotLeader::default(),
            slot,
        );
        let rank_map = bank.get_rank_map(slot).unwrap().clone();
        (0..max_validators)
            .map(|index| {
                let pubkey_affine = rank_map.get_pubkey_stake_entry(index).unwrap().bls_pubkey;
                keypair_map
                    .get(&BlsPubkeyCompressed::from(*pubkey_affine))
                    .unwrap()
                    .clone()
            })
            .collect()
    }

    #[test]
    fn validate_build_skip_cert() {
        let slot = 123;
        let max_validators = 5;
        let keypairs = get_keypairs(max_validators, slot);
        let shred_version = rand::rng().random();
        let mut entry = Entry::new(max_validators);
        let resp = entry.clone().build_certs(slot).unwrap();
        assert_eq!(resp.skip, None);
        assert_eq!(resp.notar, None);

        let skip = Vote::new_skip_vote(7);
        let (aggregate, vote_account_pubkeys) =
            new_reward_vote_aggregate(skip, 0, &keypairs, None, shred_version);
        entry
            .add_aggregate(aggregate, vote_account_pubkeys)
            .unwrap();
        let resp = entry.build_certs(slot).unwrap();
        assert_eq!(resp.notar, None);
        let skip = resp.skip.unwrap();
        assert_eq!(skip.slot, slot);
        validate_bitmap(skip.to_bitmap(), 1, 5);
    }

    #[test]
    fn validate_build_notar_cert() {
        let slot = 123;
        let max_validators = 5;
        let shred_version = rand::rng().random();
        let keypairs = get_keypairs(max_validators, slot);

        let mut entry = Entry::new(max_validators);
        let resp = entry.clone().build_certs(slot).unwrap();
        assert_eq!(resp.skip, None);
        assert_eq!(resp.notar, None);

        let blockid0 = Hash::new_unique();
        let blockid1 = Hash::new_unique();

        for rank in 0..2 {
            let notar = Vote::new_notarization_vote(Block {
                slot,
                block_id: blockid0,
            });
            let (aggregate, vote_account_pubkeys) =
                new_reward_vote_aggregate(notar, rank, &keypairs, None, shred_version);
            entry
                .add_aggregate(aggregate, vote_account_pubkeys)
                .unwrap();
        }
        for rank in 2..5 {
            let notar = Vote::new_notarization_vote(Block {
                slot,
                block_id: blockid1,
            });
            let (aggregate, vote_account_pubkeys) =
                new_reward_vote_aggregate(notar, rank, &keypairs, None, shred_version);
            entry
                .add_aggregate(aggregate, vote_account_pubkeys)
                .unwrap();
        }
        let resp = entry.build_certs(slot).unwrap();
        assert_eq!(resp.skip, None);
        let notar = resp.notar.unwrap();
        assert_eq!(notar.slot, slot);
        assert_eq!(notar.block_id, blockid1);
        validate_bitmap(notar.bitmap(), 3, 5);
    }

    #[test]
    fn identity_skip_cert_is_omitted_without_dropping_notar_cert() {
        let slot = 123;
        let max_validators = 3;
        let shred_version = rand::rng().random();
        let keypairs = (0..max_validators)
            .map(|_| BlsKeypair::new())
            .collect::<Vec<_>>();
        let mut entry = Entry::new(max_validators);

        let skip = Vote::new_skip_vote(slot);
        let (aggregate, skip_validators) = new_identity_reward_vote_aggregate(
            skip,
            max_validators,
            &[(0, 100), (1, 100)],
            shred_version,
        );
        entry.add_aggregate(aggregate, skip_validators).unwrap();

        let block_id = Hash::new_unique();
        let notar = Vote::new_notarization_vote(Block { slot, block_id });
        let (aggregate, notar_validators) =
            new_reward_vote_aggregate(notar, 2, &keypairs, None, shred_version);
        entry
            .add_aggregate(aggregate, notar_validators.clone())
            .unwrap();

        let resp = entry.build_certs(slot).unwrap();
        assert!(resp.skip.is_none());
        assert_eq!(resp.notar.unwrap().block_id, block_id);
        assert_eq!(resp.validators, notar_validators);
    }

    #[test]
    fn identity_notar_cert_is_omitted_without_dropping_skip_cert() {
        let slot = 123;
        let max_validators = 3;
        let shred_version = rand::rng().random();
        let keypairs = (0..max_validators)
            .map(|_| BlsKeypair::new())
            .collect::<Vec<_>>();
        let mut entry = Entry::new(max_validators);

        let block_id = Hash::new_unique();
        let notar = Vote::new_notarization_vote(Block { slot, block_id });
        let (aggregate, notar_validators) = new_identity_reward_vote_aggregate(
            notar,
            max_validators,
            &[(0, 100), (1, 100)],
            shred_version,
        );
        entry.add_aggregate(aggregate, notar_validators).unwrap();

        let skip = Vote::new_skip_vote(slot);
        let (aggregate, skip_validators) =
            new_reward_vote_aggregate(skip, 2, &keypairs, None, shred_version);
        entry
            .add_aggregate(aggregate, skip_validators.clone())
            .unwrap();

        let resp = entry.build_certs(slot).unwrap();
        assert!(resp.notar.is_none());
        assert_eq!(resp.skip.unwrap().slot, slot);
        assert_eq!(resp.validators, skip_validators);
    }
}
