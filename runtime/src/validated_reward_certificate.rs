use {
    crate::bank::Bank,
    agave_bls_cert_verify::cert_verify::Error as BlsCertVerifyError,
    agave_votor_messages::reward_certificate::{
        NUM_SLOTS_FOR_REWARD, NotarRewardCertificate, SkipRewardCertificate,
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
    bank: &Bank,
    skip_slot: Option<Slot>,
    notar_slot: Option<Slot>,
) -> Result<Option<Slot>, Error> {
    let current_slot = bank.slot();
    let slot = match (skip_slot, notar_slot) {
        (None, None) => return Ok(None),
        (Some(s), None) => s,
        (None, Some(n)) => n,
        (Some(skip), Some(notar)) => {
            if skip != notar {
                return Err(Error::InvalidSlotNumbers {
                    current_slot,
                    notar_slot: Some(notar),
                    skip_slot: Some(skip),
                });
            }
            skip
        }
    };
    if slot.saturating_add(NUM_SLOTS_FOR_REWARD) != current_slot
        || slot <= bank.get_alpenglow_migration_slot().unwrap_or(slot)
    {
        return Err(Error::InvalidSlotNumbers {
            current_slot: bank.slot(),
            notar_slot,
            skip_slot,
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
    pub(crate) fn try_new_validated(
        bank: &Bank,
        skip: Option<Slot>,
        notar: Option<Slot>,
        validators: HashSet<Pubkey>,
    ) -> Result<Option<Self>, Error> {
        let Some(reward_slot) = extract_slot(bank, skip, notar)? else {
            return Ok(None);
        };
        if validators.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self {
            validators,
            reward_slot,
        }))
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
    ) -> Result<Option<Self>, Error> {
        let Some(reward_slot) = extract_slot(
            bank,
            skip.as_ref().map(|s| s.slot),
            notar.as_ref().map(|n| n.slot),
        )?
        else {
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
        solana_bls_signatures::{BLS_SIGNATURE_AFFINE_SIZE, Signature as BLSSignature},
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
            let skip = Some(reward_slot);

            let result = extract_slot(&bank, skip, None);
            if reward_slot <= migration_slot {
                assert_eq!(
                    result,
                    Err(Error::InvalidSlotNumbers {
                        current_slot: bank.slot(),
                        notar_slot: None,
                        skip_slot: Some(reward_slot),
                    })
                );
            } else {
                assert_eq!(result, Ok(Some(reward_slot)));
            }
        }
    }
}
