use {
    super::*,
    spl_token_2022::{
        extension::pausable::instruction::{InitializeInstructionData, PausableInstruction},
        instruction::{decode_instruction_data, decode_instruction_type},
    },
};

pub(in crate::parse_token) fn parse_pausable_instruction(
    instruction_data: &[u8],
    account_indexes: &[u8],
    account_keys: &AccountKeys,
) -> Result<ParsedInstructionEnum, ParseInstructionError> {
    match decode_instruction_type(instruction_data)
        .map_err(|_| ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken))?
    {
        PausableInstruction::Initialize => {
            check_num_token_accounts(account_indexes, 1)?;
            let InitializeInstructionData { authority } =
                *decode_instruction_data(instruction_data).map_err(|_| {
                    ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken)
                })?;
            let authority: Option<Pubkey> = authority.into();
            Ok(ParsedInstructionEnum {
                instruction_type: "initializePausableConfig".to_string(),
                info: json!({
                    "mint": account_keys[account_indexes[0] as usize].to_string(),
                    "authority": authority.map(|pubkey| pubkey.to_string()),
                }),
            })
        }
        PausableInstruction::Pause => {
            check_num_token_accounts(account_indexes, 2)?;
            let mut value = json!({
                "mint": account_keys[account_indexes[0] as usize].to_string(),
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
                instruction_type: "pause".to_string(),
                info: value,
            })
        }
        PausableInstruction::Resume => {
            check_num_token_accounts(account_indexes, 2)?;
            let mut value = json!({
                "mint": account_keys[account_indexes[0] as usize].to_string(),
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
                instruction_type: "resume".to_string(),
                info: value,
            })
        }
    }
}
