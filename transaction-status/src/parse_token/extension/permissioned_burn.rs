use {
    super::*,
    spl_token_2022_interface::{
        extension::permissioned_burn::instruction::{
            BurnCheckedInstructionData, BurnInstructionData, InitializeInstructionData,
            PermissionedBurnInstruction,
        },
        instruction::{decode_instruction_data, decode_instruction_type},
    },
};

pub(in crate::parse_token) fn parse_permissioned_burn_instruction(
    instruction_data: &[u8],
    account_indexes: &[u8],
    account_keys: &AccountKeys,
) -> Result<ParsedInstructionEnum, ParseInstructionError> {
    match decode_instruction_type(instruction_data)
        .map_err(|_| ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken))?
    {
        PermissionedBurnInstruction::Initialize => {
            check_num_token_accounts(account_indexes, 1)?;
            let InitializeInstructionData { authority } =
                *decode_instruction_data(instruction_data).map_err(|_| {
                    ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken)
                })?;
            Ok(ParsedInstructionEnum {
                instruction_type: "initializePermissionedBurnConfig".to_string(),
                info: json!({
                    "mint": account_keys[account_indexes[0] as usize].to_string(),
                    "authority": authority.to_string(),
                }),
            })
        }
        PermissionedBurnInstruction::Burn => {
            check_num_token_accounts(account_indexes, 4)?;
            let BurnInstructionData { amount } = *decode_instruction_data(instruction_data)
                .map_err(|_| {
                    ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken)
                })?;
            let mut value = json!({
                "account": account_keys[account_indexes[0] as usize].to_string(),
                "mint": account_keys[account_indexes[1] as usize].to_string(),
                "permissionedBurnAuthority": account_keys[account_indexes[2] as usize].to_string(),
                "amount": u64::from(amount).to_string(),
            });
            let map = value.as_object_mut().unwrap();
            parse_signers(
                map,
                3,
                account_keys,
                account_indexes,
                "authority",
                "multisigAuthority",
            );
            Ok(ParsedInstructionEnum {
                instruction_type: "permissionedBurn".to_string(),
                info: value,
            })
        }
        PermissionedBurnInstruction::BurnChecked => {
            check_num_token_accounts(account_indexes, 4)?;
            let BurnCheckedInstructionData { amount, decimals } =
                *decode_instruction_data(instruction_data).map_err(|_| {
                    ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken)
                })?;
            let additional_data = SplTokenAdditionalDataV2::with_decimals(decimals);
            let mut value = json!({
                "account": account_keys[account_indexes[0] as usize].to_string(),
                "mint": account_keys[account_indexes[1] as usize].to_string(),
                "permissionedBurnAuthority": account_keys[account_indexes[2] as usize].to_string(),
                "tokenAmount": token_amount_to_ui_amount_v3(u64::from(amount), &additional_data),
            });
            let map = value.as_object_mut().unwrap();
            parse_signers(
                map,
                3,
                account_keys,
                account_indexes,
                "authority",
                "multisigAuthority",
            );
            Ok(ParsedInstructionEnum {
                instruction_type: "permissionedBurnChecked".to_string(),
                info: value,
            })
        }
        PermissionedBurnInstruction::ConfidentialBurn => Err(
            ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken),
        ),
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        solana_message::Message,
        solana_pubkey::Pubkey,
        spl_token_2022_interface::extension::permissioned_burn::instruction::{burn, initialize},
    };

    #[test]
    fn test_parse_initialize_permissioned_burn_instruction() {
        let mint = Pubkey::new_unique();
        let authority = Pubkey::new_unique();
        let ix = initialize(&spl_token_2022_interface::id(), &mint, &authority).unwrap();
        let message = Message::new(&[ix], None);
        let compiled_instruction = &message.instructions[0];
        assert_eq!(
            parse_token(
                compiled_instruction,
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "initializePermissionedBurnConfig".to_string(),
                info: json!({
                    "mint": mint.to_string(),
                    "authority": authority.to_string(),
                })
            }
        );
    }

    #[test]
    fn test_parse_permissioned_burn_instruction() {
        let account = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let permissioned_burn_authority = Pubkey::new_unique();
        let authority = Pubkey::new_unique();
        let ix = burn(
            &spl_token_2022_interface::id(),
            &account,
            &mint,
            &permissioned_burn_authority,
            &authority,
            &[],
            42,
        )
        .unwrap();
        let message = Message::new(&[ix], None);
        let compiled_instruction = &message.instructions[0];
        assert_eq!(
            parse_token(
                compiled_instruction,
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "permissionedBurn".to_string(),
                info: json!({
                    "account": account.to_string(),
                    "mint": mint.to_string(),
                    "permissionedBurnAuthority": permissioned_burn_authority.to_string(),
                    "amount": "42",
                    "authority": authority.to_string(),
                })
            }
        );
    }
}
