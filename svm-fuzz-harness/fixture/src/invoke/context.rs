//! Instruction context (input).

use {
    super::instr_account::InstrAccount,
    crate::{
        context::{epoch_context::EpochContext, slot_context::SlotContext},
        error::FixtureError,
        proto::InstrContext as ProtoInstrContext,
    },
    solana_account::AccountSharedData,
    solana_pubkey::Pubkey,
};

/// Instruction context fixture.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InstrContext {
    /// The program ID of the program being invoked.
    pub program_id: Pubkey,
    /// Input accounts with state.
    pub accounts: Vec<(Pubkey, AccountSharedData)>,
    /// Accounts to pass to the instruction.
    pub instruction_accounts: Vec<InstrAccount>,
    /// The instruction data.
    pub instruction_data: Vec<u8>,
    /// The compute units available to the program.
    pub compute_units_available: u64,
    /// Slot context.
    pub slot_context: SlotContext,
    /// Epoch context.
    pub epoch_context: EpochContext,
}

impl TryFrom<ProtoInstrContext> for InstrContext {
    type Error = FixtureError;

    fn try_from(value: ProtoInstrContext) -> Result<Self, Self::Error> {
        let program_id =
            Pubkey::try_from(value.program_id).map_err(FixtureError::InvalidPubkeyBytes)?;

        let accounts: Vec<(Pubkey, AccountSharedData)> = value
            .accounts
            .into_iter()
            .map(|a| a.try_into())
            .collect::<Result<_, _>>()?;

        let instruction_accounts: Vec<InstrAccount> = value
            .instr_accounts
            .into_iter()
            .map(|acct| {
                if acct.index as usize >= accounts.len() {
                    return Err(FixtureError::AccountMissingForInstrAccount(
                        acct.index as usize,
                    ));
                }
                Ok(InstrAccount::from(acct))
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            program_id,
            accounts,
            instruction_accounts,
            instruction_data: value.data,
            compute_units_available: value.cu_avail,
            slot_context: value.slot_context.map(Into::into).unwrap_or_default(),
            epoch_context: value.epoch_context.map(Into::into).unwrap_or_default(),
        })
    }
}

impl From<InstrContext> for ProtoInstrContext {
    fn from(value: InstrContext) -> Self {
        let accounts = value.accounts.into_iter().map(Into::into).collect();

        let instr_accounts = value
            .instruction_accounts
            .into_iter()
            .map(Into::into)
            .collect();

        Self {
            program_id: value.program_id.to_bytes().to_vec(),
            accounts,
            instr_accounts,
            data: value.instruction_data,
            cu_avail: value.compute_units_available,
            slot_context: Some(value.slot_context.into()),
            epoch_context: Some(value.epoch_context.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::proto::{AcctState as ProtoAccount, InstrAcct as ProtoInstrAccount},
        solana_account::Account,
        solana_feature_set::FeatureSet,
    };

    fn test_to_from_instr_context(instr_context: InstrContext) {
        let proto = ProtoInstrContext::from(instr_context.clone());
        let converted_instr_context = InstrContext::try_from(proto).unwrap();
        // Feature set is covered in its own tests.
        assert_eq!(instr_context.accounts, converted_instr_context.accounts);
        assert_eq!(
            instr_context.compute_units_available,
            converted_instr_context.compute_units_available
        );
        assert_eq!(
            instr_context.instruction_accounts,
            converted_instr_context.instruction_accounts
        );
        assert_eq!(
            instr_context.instruction_data,
            converted_instr_context.instruction_data
        );
        assert_eq!(instr_context.program_id, converted_instr_context.program_id);
        assert_eq!(
            instr_context.slot_context,
            converted_instr_context.slot_context
        );
    }

    #[test]
    fn test_conversion_instr_context() {
        let program_id = Pubkey::new_unique();
        let accounts = vec![(
            Pubkey::new_unique(),
            AccountSharedData::new(100, 0, &Pubkey::new_unique()),
        )];
        let instruction_accounts = vec![InstrAccount {
            index: 0,
            is_signer: true,
            is_writable: false,
        }];
        let instruction_data = vec![10, 20, 30];
        let compute_units_available = 500_000;
        let slot_context = SlotContext { slot: 50 };
        let epoch_context = EpochContext {
            feature_set: FeatureSet::default(),
        };

        test_to_from_instr_context(InstrContext {
            program_id,
            accounts,
            instruction_accounts,
            instruction_data,
            compute_units_available,
            slot_context,
            epoch_context,
        });

        let program_id = Pubkey::new_from_array([5; 32]);
        let accounts = vec![
            (
                Pubkey::new_from_array([6; 32]),
                AccountSharedData::new(1_000, 64, &Pubkey::new_from_array([7; 32])),
            ),
            (
                Pubkey::new_from_array([6; 32]),
                AccountSharedData::new(1_000, 64, &Pubkey::new_from_array([7; 32])),
            ),
        ];
        let instruction_accounts = vec![InstrAccount {
            index: 1,
            is_signer: false,
            is_writable: true,
        }];
        let instruction_data = vec![40, 50, 60];
        let compute_units_available = 1_000_000;
        let slot_context = SlotContext { slot: 150 };
        let epoch_context = EpochContext {
            feature_set: FeatureSet::default(),
        };

        test_to_from_instr_context(InstrContext {
            program_id,
            accounts,
            instruction_accounts,
            instruction_data,
            compute_units_available,
            slot_context,
            epoch_context,
        });

        let program_id = Pubkey::new_from_array([255; 32]);
        let accounts = vec![(
            Pubkey::new_from_array([254; 32]),
            AccountSharedData::from(Account {
                lamports: 500_000_000,
                data: vec![1, 2, 3, 4, 5],
                owner: Pubkey::new_from_array([253; 32]),
                executable: true,
                rent_epoch: u64::MAX,
            }),
        )];
        let instruction_accounts = vec![InstrAccount {
            index: 0,
            is_signer: true,
            is_writable: true,
        }];
        let instruction_data = vec![100, 101, 102, 103];
        let compute_units_available = u64::MAX;
        let slot_context = SlotContext { slot: 1_000 };
        let epoch_context = EpochContext {
            feature_set: FeatureSet::default(),
        };

        test_to_from_instr_context(InstrContext {
            program_id,
            accounts,
            instruction_accounts,
            instruction_data,
            compute_units_available,
            slot_context,
            epoch_context,
        });
    }

    #[test]
    fn test_conversion_instr_context_acct_missing() {
        let proto = ProtoInstrContext {
            program_id: Pubkey::new_unique().to_bytes().to_vec(),
            accounts: vec![ProtoAccount {
                address: Pubkey::new_unique().to_bytes().to_vec(),
                owner: Pubkey::new_unique().to_bytes().to_vec(),
                lamports: 100_000,
                data: vec![],
                executable: false,
                rent_epoch: 0,
                seed_addr: None,
            }],
            instr_accounts: vec![ProtoInstrAccount {
                index: 1, // <-- Out of range.
                is_signer: false,
                is_writable: true,
            }],
            data: vec![],
            cu_avail: 0,
            slot_context: None,
            epoch_context: None,
        };

        let result = InstrContext::try_from(proto);

        assert_eq!(
            result.unwrap_err(),
            FixtureError::AccountMissingForInstrAccount(1),
        );
    }
}
