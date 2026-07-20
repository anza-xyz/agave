use {
    super::BuildRewardCertsRespError,
    crate::block_creation_loop::rewards::msg_types::RewardRespSucc,
    agave_bls_sigverify::rewards::{RewardVote, RewardVoteMessage},
    agave_votor_messages::reward_certificate::SkipRewardCertificate,
    notar_entry::NotarEntry,
    partial_cert::{BuildSigBitmapError, PartialCert},
    solana_bls_signatures::BlsError,
    solana_clock::Slot,
    thiserror::Error,
};

mod notar_entry;
mod partial_cert;

/// Different types of errors that can be returned from adding votes.
#[derive(Debug, Error)]
pub(super) enum AddVoteError {
    #[error("rank on vote is invalid")]
    InvalidRank,
    #[error("duplicate vote")]
    Duplicate,
    #[error("BLS error: {0}")]
    Bls(#[from] BlsError),
}

/// Per slot container for storing notar and skip votes for creating rewards certificates.
#[derive(Clone)]
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
            notar: NotarEntry::new(max_validators),
            max_validators,
        }
    }

    /// Returns true if the [`Entry`] needs the vote else false.
    pub(super) fn wants_vote(&self, msg: &RewardVoteMessage) -> bool {
        match msg.vote {
            RewardVote::Skip(_) => self.skip.wants_vote(msg.rank),
            RewardVote::Notar(_) => self.notar.wants_vote(msg.rank),
        }
    }

    /// Adds the given [`VoteMessage`] to the aggregate.
    pub(super) fn add_vote(&mut self, msg: &RewardVoteMessage) -> Result<(), AddVoteError> {
        match &msg.vote {
            RewardVote::Notar(notar) => {
                self.notar
                    .add_vote(msg, notar.block.block_id, self.max_validators)
            }
            RewardVote::Skip(_) => self.skip.add_vote(msg),
        }
    }

    /// Builds reward certificates from the observed votes.
    pub(super) fn build_certs(
        self,
        reward_slot: Slot,
    ) -> Result<RewardRespSucc, BuildRewardCertsRespError> {
        let notar = self.notar.build_cert(reward_slot)?;
        let skip = match self.skip.build_sig_bitmap() {
            Err(e) => match e {
                BuildSigBitmapError::Empty => None,
                BuildSigBitmapError::Encode(e) => return Err(BuildRewardCertsRespError::Encode(e)),
            },
            Ok((signature, bitmap, skip_validators)) => {
                let cert = SkipRewardCertificate::try_new(reward_slot, signature, bitmap)?;
                Some((cert, skip_validators))
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
            consensus_message::Block, vote::Vote, wire::get_vote_payload_to_sign,
        },
        rand::Rng,
        solana_bls_signatures::{Keypair as BlsKeypair, PubkeyCompressed as BlsPubkeyCompressed},
        solana_epoch_schedule::EpochSchedule,
        solana_hash::Hash,
        solana_pubkey::Pubkey,
        solana_runtime::{
            bank::{Bank, SlotLeader},
            genesis_utils::{
                ValidatorVoteKeypairs, create_genesis_config_with_alpenglow_vote_accounts,
            },
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

    pub(crate) fn new_reward_vote_msg(
        vote: Vote,
        rank: usize,
        keypairs: &[BlsKeypair],
        stakes: Option<&[u64]>,
        shred_version: u16,
    ) -> RewardVoteMessage {
        let serialized = get_vote_payload_to_sign(vote, shred_version);
        let signature = keypairs[rank].sign(&serialized).into();
        let vote = match vote {
            Vote::Notarize(notar) => RewardVote::Notar(notar),
            Vote::Skip(skip) => RewardVote::Skip(skip),
            rest => panic!("unexpect vote: {rest:?}"),
        };
        let stake = match stakes {
            None => NonZero::new(123).unwrap(),
            Some(stakes) => NonZero::new(stakes[rank]).unwrap(),
        };
        RewardVoteMessage {
            vote,
            signature,
            rank: rank.try_into().unwrap(),
            stake,
            vote_account_pubkey: Pubkey::new_unique(),
        }
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
        let mut genesis_config = create_genesis_config_with_alpenglow_vote_accounts(
            1_000_000_000,
            &validator_keypairs,
            stakes,
        )
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
        let vote = new_reward_vote_msg(skip, 0, &keypairs, None, shred_version);
        entry.add_vote(&vote).unwrap();
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
            let vote = new_reward_vote_msg(notar, rank, &keypairs, None, shred_version);
            entry.add_vote(&vote).unwrap();
        }
        for rank in 2..5 {
            let notar = Vote::new_notarization_vote(Block {
                slot,
                block_id: blockid1,
            });
            let vote = new_reward_vote_msg(notar, rank, &keypairs, None, shred_version);
            entry.add_vote(&vote).unwrap();
        }
        let resp = entry.build_certs(slot).unwrap();
        assert_eq!(resp.skip, None);
        let notar = resp.notar.unwrap();
        assert_eq!(notar.slot, slot);
        assert_eq!(notar.block_id, blockid1);
        validate_bitmap(notar.bitmap(), 3, 5);
    }
}
