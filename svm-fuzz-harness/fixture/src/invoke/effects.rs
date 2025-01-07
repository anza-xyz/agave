//! Instruction effects (output).

use {
    crate::{error::FixtureError, proto::InstrEffects as ProtoInstrEffects},
    solana_account::AccountSharedData,
    solana_instruction::error::InstructionError,
    solana_pubkey::Pubkey,
};

/// Represents the effects of a single instruction.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InstrEffects {
    /// Program result. `None` is success.
    // Unfortunately, Rust doesn't implement `Default` for `Result`.
    pub program_result: Option<InstructionError>,
    /// Custom error code, if any.
    pub program_custom_code: Option<u32>,
    /// Copies of accounts that were changed.
    pub modified_accounts: Vec<(Pubkey, AccountSharedData)>,
    /// Compute units available after executing the instruction.
    pub compute_units_available: u64,
    /// Instruction return data.
    pub return_data: Vec<u8>,
}

impl TryFrom<ProtoInstrEffects> for InstrEffects {
    type Error = FixtureError;

    fn try_from(value: ProtoInstrEffects) -> Result<Self, Self::Error> {
        let ProtoInstrEffects {
            result,
            custom_err,
            modified_accounts,
            cu_avail,
            return_data,
        } = value;

        let program_result = if result == 0 {
            None
        } else {
            Some(num_to_instr_err(result, custom_err))
        };

        let program_custom_code = if custom_err == 0 {
            None
        } else {
            Some(custom_err)
        };

        let modified_accounts: Vec<(Pubkey, AccountSharedData)> = modified_accounts
            .into_iter()
            .map(|a| a.try_into())
            .collect::<Result<_, _>>()?;

        Ok(Self {
            program_result,
            program_custom_code,
            modified_accounts,
            compute_units_available: cu_avail,
            return_data,
        })
    }
}

impl From<InstrEffects> for ProtoInstrEffects {
    fn from(value: InstrEffects) -> Self {
        let InstrEffects {
            program_result,
            program_custom_code,
            modified_accounts,
            compute_units_available,
            return_data,
        } = value;

        Self {
            result: program_result
                .as_ref()
                .map(instr_err_to_num)
                .unwrap_or_default(),
            custom_err: program_custom_code.unwrap_or_default(),
            modified_accounts: modified_accounts.into_iter().map(Into::into).collect(),
            cu_avail: compute_units_available,
            return_data,
        }
    }
}

fn instr_err_to_num(error: &InstructionError) -> i32 {
    let serialized_err = bincode::serialize(error).unwrap();
    i32::from_le_bytes((&serialized_err[0..4]).try_into().unwrap()).saturating_add(1)
}

fn num_to_instr_err(num: i32, custom_code: u32) -> InstructionError {
    let val = num.saturating_sub(1) as u64;
    let le = val.to_le_bytes();
    let mut deser = bincode::deserialize(&le).unwrap();
    if custom_code != 0 && matches!(deser, InstructionError::Custom(_)) {
        deser = InstructionError::Custom(custom_code);
    }
    deser
}

#[cfg(test)]
mod tests {
    use {super::*, solana_account::Account};

    fn test_to_from_instr_effects(instr_effects: InstrEffects) {
        let proto = ProtoInstrEffects::from(instr_effects.clone());
        let converted_instr_effects = InstrEffects::try_from(proto).unwrap();
        assert_eq!(instr_effects, converted_instr_effects);
    }

    #[test]
    fn test_conversion_instr_effects() {
        let instr_effects = InstrEffects {
            program_result: None,
            program_custom_code: None,
            modified_accounts: vec![],
            compute_units_available: 1_000,
            return_data: vec![],
        };
        test_to_from_instr_effects(instr_effects);

        let modified_accounts = vec![(
            Pubkey::new_unique(),
            AccountSharedData::new(100, 0, &Pubkey::new_unique()),
        )];
        let instr_effects = InstrEffects {
            program_result: Some(InstructionError::AccountBorrowFailed),
            program_custom_code: Some(42),
            modified_accounts,
            compute_units_available: 500_000,
            return_data: vec![10, 20, 30],
        };
        test_to_from_instr_effects(instr_effects);

        let modified_accounts = vec![
            (
                Pubkey::new_from_array([1; 32]),
                AccountSharedData::new(500, 64, &Pubkey::new_from_array([2; 32])),
            ),
            (
                Pubkey::new_from_array([3; 32]),
                AccountSharedData::from(Account {
                    lamports: 1_000_000,
                    data: vec![255; 10],
                    owner: Pubkey::new_from_array([4; 32]),
                    executable: true,
                    rent_epoch: 1,
                }),
            ),
        ];
        let instr_effects = InstrEffects {
            program_result: Some(InstructionError::CallDepth),
            program_custom_code: None,
            modified_accounts,
            compute_units_available: u64::MAX,
            return_data: vec![100, 101, 102, 103, 104],
        };
        test_to_from_instr_effects(instr_effects);
    }

    #[test]
    fn test_num_to_instr_err() {
        [
            InstructionError::InvalidArgument,
            InstructionError::InvalidInstructionData,
            InstructionError::InvalidAccountData,
            InstructionError::AccountDataTooSmall,
            InstructionError::InsufficientFunds,
            InstructionError::IncorrectProgramId,
            InstructionError::MissingRequiredSignature,
            InstructionError::AccountAlreadyInitialized,
            InstructionError::UninitializedAccount,
            InstructionError::UnbalancedInstruction,
            InstructionError::ModifiedProgramId,
            InstructionError::Custom(0),
            InstructionError::Custom(1),
            InstructionError::Custom(2),
            InstructionError::Custom(5),
            InstructionError::Custom(400),
            InstructionError::Custom(600),
            InstructionError::Custom(1_000),
        ]
        .into_iter()
        .for_each(|ie| {
            let mut custom_code = 0;
            if let InstructionError::Custom(c) = &ie {
                custom_code = *c;
            }
            let result = instr_err_to_num(&ie);
            let err = num_to_instr_err(result, custom_code);
            assert_eq!(ie, err);
        })
    }
}
