use {
    crate::bank::Bank,
    agave_bls_cert_verify::cert_verify::{
        Error as BlsCertVerifyError, collect_signers_base2, verify_base2,
    },
    agave_votor_messages::{
        consensus_message::Block,
        reward_certificate::{NUM_SLOTS_FOR_REWARD, NotarRewardCertificate, SkipRewardCertificate},
        vote::Vote,
    },
    solana_bls_signatures::BlsError,
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    std::collections::HashSet,
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
}

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
#[derive(Debug, Clone)]
pub struct ValidatedRewardCert {
    /// List of validators that were present in the reward certs.
    validators: HashSet<Pubkey>,
    /// The slot the reward certs refer to
    reward_slot: Slot,
}

impl ValidatedRewardCert {
    /// If validation of the provided reward certs succeeds, returns an instance of [`ValidatedRewardCert`].
    ///
    /// The aggregate signatures on `skip` and `notar` are verified against the epoch's BLS
    /// rank map. Use this for reward certs received from the network.
    pub fn try_new(
        bank: &Bank,
        skip: &Option<SkipRewardCertificate>,
        notar: &Option<NotarRewardCertificate>,
    ) -> Result<Option<Self>, Error> {
        Self::try_new_inner(bank, skip, notar, true)
    }

    /// Same as [`try_new`](Self::try_new) but **skips** verifying the aggregate signatures.
    ///
    /// This is only sound for reward certs the local node assembled itself (in the block
    /// creation loop) from votes that were already individually signature-verified upstream;
    /// re-verifying such a cert would be redundant. Never use this for certs received from
    /// the network.
    pub fn try_new_unverified(
        bank: &Bank,
        skip: &Option<SkipRewardCertificate>,
        notar: &Option<NotarRewardCertificate>,
    ) -> Result<Option<Self>, Error> {
        Self::try_new_inner(bank, skip, notar, false)
    }

    fn try_new_inner(
        bank: &Bank,
        skip: &Option<SkipRewardCertificate>,
        notar: &Option<NotarRewardCertificate>,
        verify: bool,
    ) -> Result<Option<Self>, Error> {
        let Some(reward_slot) = extract_slot(bank.slot(), skip, notar)? else {
            return Ok(None);
        };
        let rank_map = bank
            .epoch_stakes_from_slot(reward_slot)
            .ok_or(Error::NoRankMap)?
            .bls_pubkey_to_rank_map();
        let max_validators = rank_map.len();
        let mut validators = HashSet::with_capacity(max_validators);

        let mut rank_map = |ind: usize| {
            rank_map.get_pubkey_stake_entry(ind).map(|entry| {
                validators.insert(entry.vote_account_pubkey);
                entry.bls_pubkey
            })
        };

        if let Some(skip) = skip {
            if verify {
                let vote = Vote::new_skip_vote(skip.slot);
                // unwrap should be safe as we constructed the vote ourselves.
                let payload = bincode::serialize(&vote).unwrap();
                verify_base2(
                    &payload,
                    &skip.signature,
                    skip.to_bitmap(),
                    max_validators,
                    &mut rank_map,
                )?
            } else {
                collect_signers_base2(skip.to_bitmap(), max_validators, &mut rank_map)?
            }
        }
        if let Some(notar) = notar {
            if verify {
                let vote = Vote::new_notarization_vote(Block {
                    slot: notar.slot,
                    block_id: notar.block_id,
                });
                // unwrap should be safe as we constructed the vote ourselves.
                let payload = bincode::serialize(&vote).unwrap();
                verify_base2(
                    &payload,
                    &notar.signature,
                    notar.bitmap(),
                    max_validators,
                    rank_map,
                )?
            } else {
                collect_signers_base2(notar.bitmap(), max_validators, rank_map)?
            }
        }
        if validators.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self {
            validators,
            reward_slot,
        }))
    }

    pub(crate) fn slot(&self) -> Slot {
        self.reward_slot
    }

    pub(crate) fn validators(&self) -> &HashSet<Pubkey> {
        &self.validators
    }

    #[cfg(test)]
    pub(crate) fn new_for_tests(reward_slot: Slot, validators: Vec<Pubkey>) -> Self {
        let validators = validators.into_iter().collect();
        Self {
            reward_slot,
            validators,
        }
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
            Keypair as BlsKeypair, Signature as BLSSignature,
            SignatureCompressed as BlsSignatureCompressed, SignatureProjective,
            pubkey::PubkeyCompressed as BLSPubkeyCompressed,
        },
        solana_hash::Hash,
        solana_leader_schedule::SlotLeader,
        solana_signer_store::encode_base2,
        std::collections::HashMap,
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

    /// Creates a bank whose parent slot is the reward slot and returns the per-rank BLS
    /// signing keypairs, ordered so that `signing_keys[rank]` signs as the validator at
    /// that rank.
    fn setup(
        reward_slot: Slot,
        num_notar_validators: usize,
        num_skip_validators: usize,
    ) -> (Bank, Vec<BlsKeypair>) {
        let bank_slot = reward_slot + NUM_SLOTS_FOR_REWARD;
        let num_validators = num_skip_validators + num_notar_validators;

        let validator_keypairs = (0..num_validators)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        let keypair_map = validator_keypairs
            .iter()
            .map(|k| {
                (
                    BLSPubkeyCompressed::from(*k.bls_keypair.public),
                    k.bls_keypair.clone(),
                )
            })
            .collect::<HashMap<_, _>>();
        let genesis = create_genesis_config_with_alpenglow_vote_accounts(
            1_000_000_000,
            &validator_keypairs,
            vec![100; validator_keypairs.len()],
        );
        let (bank, _bank_forks) =
            Bank::new_for_tests(&genesis.genesis_config).wrap_with_bank_forks_for_tests();
        let bank = Bank::new_from_parent(bank, SlotLeader::default(), bank_slot);

        let signing_keys = {
            let rank_map = bank
                .epoch_stakes_from_slot(reward_slot)
                .unwrap()
                .bls_pubkey_to_rank_map();
            (0..num_validators)
                .map(|index| {
                    let pubkey_affine = rank_map.get_pubkey_stake_entry(index).unwrap().bls_pubkey;
                    keypair_map
                        .get(&BLSPubkeyCompressed::from(*pubkey_affine))
                        .unwrap()
                        .clone()
                })
                .collect::<Vec<_>>()
        };
        (bank, signing_keys)
    }

    #[test]
    fn validate_try_new() {
        let reward_slot = 1;
        let num_skip_validators = 3;
        let num_notar_validators = 5;
        let num_validators = num_skip_validators + num_notar_validators;
        let (bank, signing_keys) = setup(reward_slot, num_notar_validators, num_skip_validators);

        let block_id = Hash::new_unique();
        let notar_vote = Vote::new_notarization_vote(Block {
            slot: reward_slot,
            block_id,
        });
        let notar_votes = (0..num_notar_validators)
            .map(|rank| new_vote(notar_vote, rank, &signing_keys[rank]))
            .collect::<Vec<_>>();
        let (signature, bitmap) = build_sig_bitmap(&notar_votes);
        let notar_reward_cert =
            NotarRewardCertificate::try_new(reward_slot, block_id, signature, bitmap).unwrap();

        let skip_vote = Vote::new_skip_vote(reward_slot);
        let skip_votes = (num_notar_validators..num_validators)
            .map(|rank| new_vote(skip_vote, rank, &signing_keys[rank]))
            .collect::<Vec<_>>();
        let (signature, bitmap) = build_sig_bitmap(&skip_votes);
        let skip_reward_cert =
            SkipRewardCertificate::try_new(reward_slot, signature, bitmap).unwrap();

        let validated_reward_cert =
            ValidatedRewardCert::try_new(&bank, &Some(skip_reward_cert), &Some(notar_reward_cert))
                .unwrap()
                .unwrap();
        assert_eq!(validated_reward_cert.validators.len(), num_validators);
    }

    #[test]
    fn validate_try_new_unverified() {
        let reward_slot = 1;
        let num_skip_validators = 3;
        let num_notar_validators = 5;
        let (bank, signing_keys) = setup(reward_slot, num_notar_validators, num_skip_validators);

        let block_id = Hash::new_unique();
        let notar_vote = Vote::new_notarization_vote(Block {
            slot: reward_slot,
            block_id,
        });

        // A correctly signed notar reward cert.
        let notar_votes = (0..num_notar_validators)
            .map(|rank| new_vote(notar_vote, rank, &signing_keys[rank]))
            .collect::<Vec<_>>();
        let (signature, bitmap) = build_sig_bitmap(&notar_votes);
        let valid_cert =
            NotarRewardCertificate::try_new(reward_slot, block_id, signature, bitmap).unwrap();

        // For a valid cert, the unverified path collects exactly the same signer set and
        // reward slot as the verified path.
        let verified = ValidatedRewardCert::try_new(&bank, &None, &Some(valid_cert.clone()))
            .unwrap()
            .unwrap();
        let unverified = ValidatedRewardCert::try_new_unverified(&bank, &None, &Some(valid_cert))
            .unwrap()
            .unwrap();
        assert_eq!(unverified.reward_slot, verified.reward_slot);
        assert_eq!(unverified.validators, verified.validators);
        assert_eq!(unverified.validators.len(), num_notar_validators);

        // A cert with the same (valid) bitmap but an aggregate signature produced by the
        // WRONG keys: the verified path must reject it, while the unverified path must accept
        // it and still collect the signing validators from the bitmap.
        let wrong_keys = (0..num_notar_validators)
            .map(|_| BlsKeypair::new())
            .collect::<Vec<_>>();
        let forged_votes = (0..num_notar_validators)
            .map(|rank| new_vote(notar_vote, rank, &wrong_keys[rank]))
            .collect::<Vec<_>>();
        let (bad_signature, bitmap) = build_sig_bitmap(&forged_votes);
        let forged_cert =
            NotarRewardCertificate::try_new(reward_slot, block_id, bad_signature, bitmap).unwrap();

        assert!(matches!(
            ValidatedRewardCert::try_new(&bank, &None, &Some(forged_cert.clone())),
            Err(Error::BlsCertVerify(_))
        ));
        let unverified_forged =
            ValidatedRewardCert::try_new_unverified(&bank, &None, &Some(forged_cert))
                .unwrap()
                .unwrap();
        assert_eq!(unverified_forged.validators.len(), num_notar_validators);
    }
}
