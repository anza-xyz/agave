use {
    crate::bank::Bank,
    agave_bls_cert_verify::cert_verify::{Error as BlsCertVerifyError, verify_base2},
    agave_votor_messages::{
        reward_certificate::{NUM_SLOTS_FOR_REWARD, NotarRewardCertificate, SkipRewardCertificate},
        vote::Vote,
    },
    solana_bls_signatures::BlsError,
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    thiserror::Error,
};

/// Different types of errors that can happen when trying to construct a [`ValidatedRewardCert`].
#[derive(Debug, PartialEq, Eq, Error)]
pub enum Error {
    #[error(
        "skip or notar certs have invalid slot numbers: cur={current_slot}, notar={notar_slot:?}, \
         skip={skip_slot:?}"
    )]
    InvalidSlotNumbers {
        current_slot: Slot,
        notar_slot: Option<Slot>,
        skip_slot: Option<Slot>,
    },
    #[error("rank map unavailable")]
    NoRankMap,
    #[error("bls cert verification failed with {0}")]
    BlsCertVerify(#[from] BlsCertVerifyError),
    #[error("verify signature failed with {0:?}")]
    VerifySig(#[from] BlsError),
    #[error("empty certs were provided")]
    Empty,
}

#[allow(dead_code)]
/// Extracts the slot corresponding to the provided reward certs.
///
/// Returns Ok(None) if no certs were provided.
/// Returns Error if the reward slot is invalid.
fn extract_slot(
    current_slot: Slot,
    skip: &Option<SkipRewardCertificate>,
    notar: &Option<NotarRewardCertificate>,
) -> Result<Option<Slot>, Error> {
    let slot = match (skip, notar) {
        (None, None) => return Ok(None),
        (Some(s), None) => s.slot,
        (None, Some(n)) => n.slot,
        (Some(s), Some(n)) => {
            if s.slot != n.slot {
                return Err(Error::InvalidSlotNumbers {
                    current_slot,
                    notar_slot: Some(n.slot),
                    skip_slot: Some(s.slot),
                });
            }
            s.slot
        }
    };
    if slot.saturating_add(NUM_SLOTS_FOR_REWARD) != current_slot {
        return Err(Error::InvalidSlotNumbers {
            current_slot,
            notar_slot: notar.as_ref().map(|c| c.slot),
            skip_slot: skip.as_ref().map(|c| c.slot),
        });
    }
    Ok(Some(slot))
}

/// Struct built by validating incoming reward certs.
#[allow(dead_code)]
pub(crate) struct ValidatedRewardCert {
    /// List of validators that were present in the reward certs.
    validators: Vec<Pubkey>,
    /// The slot the reward certs refer to
    reward_slot: Slot,
}

impl ValidatedRewardCert {
    /// If validation of the provided reward certs succeeds, returns an instance of [`ValidatedRewardCert`].
    #[allow(dead_code)]
    pub(crate) fn try_new(
        bank: &Bank,
        skip: &Option<SkipRewardCertificate>,
        notar: &Option<NotarRewardCertificate>,
    ) -> Result<Self, Error> {
        let Some(reward_slot) = extract_slot(bank.slot(), skip, notar)? else {
            return Err(Error::Empty);
        };
        let rank_map = bank
            .epoch_stakes_from_slot(reward_slot)
            .ok_or(Error::NoRankMap)?
            .bls_pubkey_to_rank_map();
        let max_validators = rank_map.len();
        let mut validators = Vec::with_capacity(max_validators);

        let mut rank_map = |ind: usize| {
            rank_map.get_pubkey_stake_entry(ind).map(|entry| {
                validators.push(entry.pubkey);
                entry.bls_pubkey
            })
        };

        if let Some(skip) = skip {
            let vote = Vote::new_skip_vote(skip.slot);
            // unwrap should be safe as we contructed the vote ourselves.
            let payload = bincode::serialize(&vote).unwrap();
            verify_base2(
                &payload,
                &skip.signature,
                skip.to_bitmap(),
                max_validators,
                &mut rank_map,
            )?
        }
        if let Some(notar) = notar {
            let vote = Vote::new_notarization_vote(notar.slot, notar.block_id);
            // unwrap should be safe as we contructed the vote ourselves.
            let payload = bincode::serialize(&vote).unwrap();
            verify_base2(
                &payload,
                &notar.signature,
                notar.bitmap(),
                max_validators,
                rank_map,
            )?
        }
        if validators.is_empty() {
            return Err(Error::Empty);
        }
        Ok(Self {
            validators,
            reward_slot,
        })
    }

    /// Returns the validators that were extracted from the reward certs.
    #[allow(dead_code)]
    pub(crate) fn into_parts(self) -> (Slot, Vec<Pubkey>) {
        (self.reward_slot, self.validators)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::genesis_utils::{
            ValidatorVoteKeypairs, create_genesis_config_with_alpenglow_vote_accounts,
        },
        agave_votor_messages::consensus_message::VoteMessage,
        bitvec::vec::BitVec,
        solana_bls_signatures::{
            Keypair as BlsKeypair, Pubkey as BLSPubkey, Signature as BLSSignature,
            SignatureCompressed as BlsSignatureCompressed, SignatureProjective,
        },
        solana_hash::Hash,
        solana_signer_store::encode_base2,
        std::{collections::HashMap, sync::Arc},
    };

    fn new_vote(vote: Vote, rank: usize, keypair: &BlsKeypair) -> VoteMessage {
        let serialized = bincode::serialize(&vote).unwrap();
        let signature = keypair.sign(&serialized).into();
        VoteMessage {
            vote,
            signature,
            rank: rank.try_into().unwrap(),
        }
    }

    fn build_sig_bitmap(votes: &[VoteMessage]) -> (BlsSignatureCompressed, Vec<u8>) {
        let max_rank = votes.last().unwrap().rank;
        let mut signature = SignatureProjective::identity();
        let mut bitvec = BitVec::repeat(false, (max_rank + 1) as usize);
        for vote in votes {
            signature
                .aggregate_with(std::iter::once(&vote.signature))
                .unwrap();
            bitvec.set(vote.rank as usize, true);
        }
        (
            BLSSignature::from(signature).try_into().unwrap(),
            encode_base2(&bitvec).unwrap(),
        )
    }

    #[test]
    fn validate_try_new() {
        let reward_slot = 1;
        let bank_slot = reward_slot + NUM_SLOTS_FOR_REWARD;
        let num_skip_validators = 3;
        let num_notar_validators = 5;
        let num_validators = num_skip_validators + num_notar_validators;

        let validator_keypairs = (0..num_validators)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        let keypair_map = validator_keypairs
            .iter()
            .map(|k| (BLSPubkey::from(k.bls_keypair.public), k.bls_keypair.clone()))
            .collect::<HashMap<_, _>>();
        let genesis = create_genesis_config_with_alpenglow_vote_accounts(
            1_000_000_000,
            &validator_keypairs,
            vec![100; validator_keypairs.len()],
        );
        let bank = Arc::new(Bank::new_for_tests(&genesis.genesis_config));
        let bank = Bank::new_from_parent(bank, &Pubkey::default(), bank_slot);

        let rank_map = bank
            .epoch_stakes_from_slot(reward_slot)
            .unwrap()
            .bls_pubkey_to_rank_map();
        let signing_keys = (0..num_validators)
            .map(|index| {
                keypair_map
                    .get(&rank_map.get_pubkey_stake_entry(index).unwrap().bls_pubkey)
                    .unwrap()
            })
            .collect::<Vec<_>>();

        let blockid = Hash::new_unique();
        let notar_vote = Vote::new_notarization_vote(reward_slot, blockid);
        let notar_votes = (0..num_notar_validators)
            .map(|rank| new_vote(notar_vote, rank, signing_keys[rank]))
            .collect::<Vec<_>>();
        let (signature, bitmap) = build_sig_bitmap(&notar_votes);
        let notar_reward_cert =
            NotarRewardCertificate::try_new(reward_slot, blockid, signature, bitmap).unwrap();

        let skip_vote = Vote::new_skip_vote(reward_slot);
        let skip_votes = (num_notar_validators..num_validators)
            .map(|rank| new_vote(skip_vote, rank, signing_keys[rank]))
            .collect::<Vec<_>>();
        let (signature, bitmap) = build_sig_bitmap(&skip_votes);
        let skip_reward_cert =
            SkipRewardCertificate::try_new(reward_slot, signature, bitmap).unwrap();

        let validated_reward_cert =
            ValidatedRewardCert::try_new(&bank, &Some(skip_reward_cert), &Some(notar_reward_cert))
                .unwrap();
        assert_eq!(validated_reward_cert.validators.len(), num_validators);
    }
}
