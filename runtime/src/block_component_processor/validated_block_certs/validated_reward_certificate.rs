use {
    crate::bank::Bank,
    agave_votor_messages::reward_certificate::{
        NUM_SLOTS_FOR_REWARD, NotarRewardCertificate, SkipRewardCertificate,
    },
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    std::collections::HashSet,
};

#[derive(Debug, PartialEq, Eq)]
pub struct InvalidSlots {
    pub current_slot: Slot,
    pub notar_slot: Option<Slot>,
    pub skip_slot: Option<Slot>,
}

/// Extracts the slot corresponding to the provided reward certs.
///
/// Returns Ok(None) if no certs were provided.
/// Returns Error if the reward slot is invalid.
pub(super) fn extract_slot(
    bank: &Bank,
    skip: &Option<SkipRewardCertificate>,
    notar: &Option<NotarRewardCertificate>,
) -> Result<Option<Slot>, InvalidSlots> {
    let current_slot = bank.slot();
    let slot = match (skip, notar) {
        (None, None) => return Ok(None),
        (Some(s), None) => s.slot,
        (None, Some(n)) => n.slot,
        (Some(s), Some(n)) => {
            if s.slot != n.slot {
                return Err(InvalidSlots {
                    current_slot,
                    notar_slot: Some(n.slot),
                    skip_slot: Some(s.slot),
                });
            }
            s.slot
        }
    };
    if slot.saturating_add(NUM_SLOTS_FOR_REWARD) != current_slot
        || slot <= bank.get_alpenglow_migration_slot().unwrap_or(slot)
    {
        return Err(InvalidSlots {
            current_slot: bank.slot(),
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
    pub(super) fn new_validated(validators: HashSet<Pubkey>, reward_slot: Slot) -> Self {
        Self {
            reward_slot,
            validators,
        }
    }

    /// Constructs a [`ValidatedRewardCert`] for a block produced locally.
    ///
    /// The leader-side reward certificate builder receives verified votes and
    /// tracks the validator set while aggregating them, so block production
    /// only needs the reward slot and validator set for bank reward
    /// calculation.
    pub fn try_new_for_leader(
        bank: &Bank,
        skip: &Option<SkipRewardCertificate>,
        notar: &Option<NotarRewardCertificate>,
        validators: impl IntoIterator<Item = Pubkey>,
    ) -> Result<Option<Self>, InvalidSlots> {
        let Some(reward_slot) = extract_slot(bank, skip, notar)? else {
            return Ok(None);
        };
        let validators: HashSet<_> = validators.into_iter().collect();
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
        crate::genesis_utils::{ValidatorVoteKeypairs, create_genesis_config_with_vote_accounts},
        agave_votor_messages::{
            certificate::{CertSignature, GenesisCert},
            consensus_message::Block,
        },
        solana_bls_signatures::{
            BLS_SIGNATURE_AFFINE_SIZE, BLS_SIGNATURE_COMPRESSED_SIZE, Signature as BLSSignature,
            SignatureCompressed as BlsSignatureCompressed,
        },
        solana_leader_schedule::SlotLeader,
    };

    #[test]
    fn test_extract_slot_rejects_tower_slots() {
        let migration_slot = 1;
        let validator_keypairs = [ValidatorVoteKeypairs::new_rand()];
        let genesis =
            create_genesis_config_with_vote_accounts(1_000_000_000, &validator_keypairs, vec![100]);
        let (root_bank, _bank_forks) =
            Bank::new_for_tests(&genesis.genesis_config).wrap_with_bank_forks_for_tests();
        let genesis_cert = GenesisCert {
            block: Block::new_unique(migration_slot),
            signature: CertSignature {
                signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
                bitmap: vec![],
            },
        };

        for reward_slot in [migration_slot - 1, migration_slot, migration_slot + 1] {
            let bank = Bank::new_from_parent(
                root_bank.clone(),
                SlotLeader::default(),
                reward_slot + NUM_SLOTS_FOR_REWARD,
            );
            bank.set_alpenglow_genesis_certificate(&genesis_cert);
            let skip = Some(
                SkipRewardCertificate::try_new(
                    reward_slot,
                    BlsSignatureCompressed([0; BLS_SIGNATURE_COMPRESSED_SIZE]),
                    vec![],
                )
                .unwrap(),
            );

            let result = extract_slot(&bank, &skip, &None);
            if reward_slot <= migration_slot {
                assert_eq!(
                    result.unwrap_err(),
                    InvalidSlots {
                        current_slot: bank.slot(),
                        notar_slot: None,
                        skip_slot: Some(reward_slot),
                    }
                );
            } else {
                assert_eq!(result, Ok(Some(reward_slot)));
            }
        }
    }
}
