use {
    super::*,
    spl_token_2022::{
        extension::scaled_ui_amount::instruction::{
            InitializeInstructionData, ScaledUiAmountMintInstruction,
            UpdateMultiplierInstructionData,
        },
        instruction::{decode_instruction_data, decode_instruction_type},
    },
};

pub(in crate::parse_token) fn parse_scaled_ui_amount_instruction(
    instruction_data: &[u8],
    account_indexes: &[u8],
    account_keys: &AccountKeys,
) -> Result<ParsedInstructionEnum, ParseInstructionError> {
    match decode_instruction_type(instruction_data)
        .map_err(|_| ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken))?
    {
        ScaledUiAmountMintInstruction::Initialize => {
            check_num_token_accounts(account_indexes, 1)?;
            let InitializeInstructionData {
                authority,
                multiplier,
            } = *decode_instruction_data(instruction_data).map_err(|_| {
                ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken)
            })?;
            let authority: Option<Pubkey> = authority.into();
            Ok(ParsedInstructionEnum {
                instruction_type: "initializeScaledUiAmountConfig".to_string(),
                info: json!({
                    "mint": account_keys[account_indexes[0] as usize].to_string(),
                    "authority": authority.map(|pubkey| pubkey.to_string()),
                    "multiplier": f64::from(multiplier).to_string(),
                }),
            })
        }
        ScaledUiAmountMintInstruction::UpdateMultiplier => {
            check_num_token_accounts(account_indexes, 2)?;
            let UpdateMultiplierInstructionData {
                multiplier,
                effective_timestamp,
            } = *decode_instruction_data(instruction_data).map_err(|_| {
                ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken)
            })?;
            let mut value = json!({
                "mint": account_keys[account_indexes[0] as usize].to_string(),
                "newMultiplier": f64::from(multiplier).to_string(),
                "newMultiplierTimestamp": i64::from(effective_timestamp),
            });
            let map = value.as_object_mut().unwrap();
            parse_signers(
                map,
                1,
                account_keys,
                account_indexes,
                "authority",
                "multisigAuthority",
            );
            Ok(ParsedInstructionEnum {
                instruction_type: "updateMultiplier".to_string(),
                info: value,
            })
        }
    }
}
