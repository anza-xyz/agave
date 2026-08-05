use {
    super::*,
    solana_zk_sdk_pod::encryption::elgamal::PodElGamalPubkey,
    spl_token_2022_interface::{
        extension::confidential_transfer_fee::instruction::*,
        instruction::{decode_instruction_data, decode_instruction_type},
    },
};

pub(in crate::parse_token) fn parse_confidential_transfer_fee_instruction(
    instruction_data: &[u8],
    account_indexes: &[u8],
    account_keys: &AccountKeys,
) -> Result<ParsedInstructionEnum, ParseInstructionError> {
    match decode_instruction_type(instruction_data)
        .map_err(|_| ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken))?
    {
        ConfidentialTransferFeeInstruction::InitializeConfidentialTransferFeeConfig => {
            check_num_token_accounts(account_indexes, 1)?;
            let transfer_fee_config: InitializeConfidentialTransferFeeConfigData =
                *decode_instruction_data(instruction_data).map_err(|_| {
                    ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken)
                })?;
            let mut value = json!({
                "mint": account_keys[account_indexes[0] as usize].to_string(),
                "withdrawWithheldAuthorityElGamalPubkey": Option::<PodElGamalPubkey>::from(transfer_fee_config.withdraw_withheld_authority_elgamal_pubkey).map(|k| k.to_string()),
            });
            let map = value.as_object_mut().unwrap();
            if let Some(authority) = Option::<Pubkey>::from(transfer_fee_config.authority) {
                map.insert("authority".to_string(), json!(authority.to_string()));
            }
            Ok(ParsedInstructionEnum {
                instruction_type: "initializeConfidentialTransferFeeConfig".to_string(),
                info: value,
            })
        }
        ConfidentialTransferFeeInstruction::WithdrawWithheldTokensFromMint => {
            check_num_token_accounts(account_indexes, 4)?;
            let withdraw_withheld_data: WithdrawWithheldTokensFromMintData =
                *decode_instruction_data(instruction_data).map_err(|_| {
                    ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken)
                })?;
            let proof_instruction_offset: i8 = withdraw_withheld_data.proof_instruction_offset;
            let mut value = json!({
                "mint": account_keys[account_indexes[0] as usize].to_string(),
                "feeRecipient": account_keys[account_indexes[1] as usize].to_string(),
                "proofInstructionOffset": proof_instruction_offset,
                "newDecryptableAvailableBalance": format!("{}", withdraw_withheld_data.new_decryptable_available_balance),
            });
            let map = value.as_object_mut().unwrap();
            let mut offset = 2;
            if offset < account_indexes.len().saturating_sub(1) {
                if proof_instruction_offset == 0 {
                    map.insert(
                        "proofContextStateAccount".to_string(),
                        json!(account_keys[account_indexes[offset] as usize].to_string()),
                    );
                } else {
                    map.insert(
                        "instructionsSysvar".to_string(),
                        json!(account_keys[account_indexes[offset] as usize].to_string()),
                    );
                }
                offset += 1;
            }

            parse_signers(
                map,
                offset,
                account_keys,
                account_indexes,
                "withdrawWithheldAuthority",
                "multisigWithdrawWithheldAuthority",
            );
            Ok(ParsedInstructionEnum {
                instruction_type: "withdrawWithheldConfidentialTransferTokensFromMint".to_string(),
                info: value,
            })
        }
        ConfidentialTransferFeeInstruction::WithdrawWithheldTokensFromAccounts => {
            let withdraw_withheld_data: WithdrawWithheldTokensFromAccountsData =
                *decode_instruction_data(instruction_data).map_err(|_| {
                    ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken)
                })?;
            let num_token_accounts = withdraw_withheld_data.num_token_accounts;
            check_num_token_accounts(account_indexes, 4 + num_token_accounts as usize)?;
            let proof_instruction_offset: i8 = withdraw_withheld_data.proof_instruction_offset;
            let mut value = json!({
                "mint": account_keys[account_indexes[0] as usize].to_string(),
                "feeRecipient": account_keys[account_indexes[1] as usize].to_string(),
                "proofInstructionOffset": proof_instruction_offset,
                "newDecryptableAvailableBalance": format!("{}", withdraw_withheld_data.new_decryptable_available_balance),
            });
            let map = value.as_object_mut().unwrap();
            let first_source_account_index = account_indexes
                .len()
                .saturating_sub(num_token_accounts as usize);
            if proof_instruction_offset == 0 {
                map.insert(
                    "proofContextStateAccount".to_string(),
                    json!(account_keys[account_indexes[2] as usize].to_string()),
                );
            } else {
                map.insert(
                    "instructionsSysvar".to_string(),
                    json!(account_keys[account_indexes[2] as usize].to_string()),
                );
            }
            let mut source_accounts: Vec<String> = vec![];
            for i in account_indexes[first_source_account_index..].iter() {
                source_accounts.push(account_keys[*i as usize].to_string());
            }
            map.insert("sourceAccounts".to_string(), json!(source_accounts));
            parse_signers(
                map,
                3,
                account_keys,
                &account_indexes[..first_source_account_index],
                "withdrawWithheldAuthority",
                "multisigWithdrawWithheldAuthority",
            );
            Ok(ParsedInstructionEnum {
                instruction_type: "withdrawWithheldConfidentialTransferTokensFromAccounts"
                    .to_string(),
                info: value,
            })
        }
        ConfidentialTransferFeeInstruction::HarvestWithheldTokensToMint => {
            check_num_token_accounts(account_indexes, 1)?;
            let mut value = json!({
                "mint": account_keys[account_indexes[0] as usize].to_string(),

            });
            let map = value.as_object_mut().unwrap();
            let mut source_accounts: Vec<String> = vec![];
            for i in account_indexes.iter().skip(1) {
                source_accounts.push(account_keys[*i as usize].to_string());
            }
            map.insert("sourceAccounts".to_string(), json!(source_accounts));
            Ok(ParsedInstructionEnum {
                instruction_type: "harvestWithheldConfidentialTransferTokensToMint".to_string(),
                info: value,
            })
        }
        ConfidentialTransferFeeInstruction::EnableHarvestToMint => {
            check_num_token_accounts(account_indexes, 2)?;
            let mut value = json!({
                "account": account_keys[account_indexes[0] as usize].to_string(),

            });
            let map = value.as_object_mut().unwrap();
            parse_signers(
                map,
                1,
                account_keys,
                account_indexes,
                "owner",
                "multisigOwner",
            );
            Ok(ParsedInstructionEnum {
                instruction_type: "enableConfidentialTransferFeeHarvestToMint".to_string(),
                info: value,
            })
        }
        ConfidentialTransferFeeInstruction::DisableHarvestToMint => {
            check_num_token_accounts(account_indexes, 2)?;
            let mut value = json!({
                "account": account_keys[account_indexes[0] as usize].to_string(),

            });
            let map = value.as_object_mut().unwrap();
            parse_signers(
                map,
                1,
                account_keys,
                account_indexes,
                "owner",
                "multisigOwner",
            );
            Ok(ParsedInstructionEnum {
                instruction_type: "disableConfidentialTransferFeeHarvestToMint".to_string(),
                info: value,
            })
        }
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        bytemuck::Zeroable,
        solana_instruction::{AccountMeta, Instruction},
        solana_message::Message,
        solana_pubkey::Pubkey,
        solana_sdk_ids::sysvar,
        solana_zk_sdk_pod::encryption::auth_encryption::PodAeCiphertext,
        spl_token_2022_interface::{
            extension::confidential_transfer_fee::instruction::{
                inner_withdraw_withheld_tokens_from_accounts,
                inner_withdraw_withheld_tokens_from_mint,
            },
            solana_zk_elgamal_proof_interface::proof_data::CiphertextCiphertextEqualityProofData,
        },
        spl_token_confidential_transfer_proof_extraction::instruction::ProofLocation,
        std::num::NonZero,
    };

    fn check_no_panic(mut instruction: Instruction) {
        let account_meta = AccountMeta::new_readonly(Pubkey::new_unique(), false);
        for i in 0..20 {
            instruction.accounts = vec![account_meta.clone(); i];
            let message = Message::new(&[instruction.clone()], None);
            let compiled_instruction = &message.instructions[0];
            let _ = parse_token(
                compiled_instruction,
                &AccountKeys::new(&message.account_keys, None),
            );
        }
    }

    #[test]
    fn test_withdraw_from_mint() {
        let mint = Pubkey::new_unique();
        let destination = Pubkey::new_unique();
        let authority = Pubkey::new_unique();
        let proof_ctx = Pubkey::new_unique();

        let new_decryptable_balance = PodAeCiphertext::default();

        let offset_proof = ProofLocation::InstructionOffset(
            NonZero::new(1).unwrap(),
            &CiphertextCiphertextEqualityProofData::zeroed(),
        );

        let context_proof = ProofLocation::ContextStateAccount(&proof_ctx);

        // Array of cases: (Test Name, Proof Location, Expected Offset)
        let cases = vec![
            ("Context State Account", context_proof, 0),
            ("Instruction Offset", offset_proof, 1),
        ];

        for (name, proof_location, expected_offset) in cases {
            let instruction = inner_withdraw_withheld_tokens_from_mint(
                &spl_token_2022_interface::id(),
                &mint,
                &destination,
                &new_decryptable_balance,
                &authority,
                &[],
                proof_location,
            )
            .unwrap();

            check_no_panic(instruction.clone());

            let message = Message::new(&[instruction], None);
            let parsed = parse_token(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None),
            )
            .unwrap();

            // Core Property Assertions
            assert_eq!(
                parsed.instruction_type, "withdrawWithheldConfidentialTransferTokensFromMint",
                "Failed on: {name}",
            );
            assert_eq!(
                parsed.info["mint"],
                json!(mint.to_string()),
                "Failed on: {name}",
            );
            assert_eq!(
                parsed.info["feeRecipient"],
                json!(destination.to_string()),
                "Failed on: {name}",
            );
            assert_eq!(
                parsed.info["withdrawWithheldAuthority"],
                json!(authority.to_string()),
                "Failed on: {name}",
            );
            assert_eq!(
                parsed.info["newDecryptableAvailableBalance"],
                json!(format!("{new_decryptable_balance}")),
                "Failed on: {name}",
            );

            assert_eq!(
                parsed.info["proofInstructionOffset"],
                json!(expected_offset),
                "Failed on: {name}",
            );

            if expected_offset == 0 {
                assert_eq!(
                    parsed.info["proofContextStateAccount"],
                    json!(proof_ctx.to_string()),
                    "Proof Context mismatch on: {name}",
                );
                assert!(
                    parsed.info.get("instructionsSysvar").is_none(),
                    "Sysvar should not be present on: {name}",
                );
            } else {
                assert_eq!(
                    parsed.info["instructionsSysvar"],
                    json!(sysvar::instructions::id().to_string()),
                    "Sysvar mismatch on: {name}",
                );
                assert!(
                    parsed.info.get("proofContextStateAccount").is_none(),
                    "Proof Context should not be present on: {name}",
                );
            }
        }
    }

    #[test]
    fn test_withdraw_from_accounts() {
        let mint = Pubkey::new_unique();
        let destination = Pubkey::new_unique();
        let authority = Pubkey::new_unique();
        let source_1 = Pubkey::new_unique();
        let source_2 = Pubkey::new_unique();
        let proof_ctx = Pubkey::new_unique();

        let new_decryptable_balance = PodAeCiphertext::default();

        let offset_proof = ProofLocation::InstructionOffset(
            NonZero::new(1).unwrap(),
            &CiphertextCiphertextEqualityProofData::zeroed(),
        );

        let context_proof = ProofLocation::ContextStateAccount(&proof_ctx);

        // Array of cases: (Test Name, Proof Location, Expected Offset)
        let cases = vec![
            ("Context State Account", context_proof, 0),
            ("Instruction Offset", offset_proof, 1),
        ];

        for (name, proof_location, expected_offset) in cases {
            let instruction = inner_withdraw_withheld_tokens_from_accounts(
                &spl_token_2022_interface::id(),
                &mint,
                &destination,
                &new_decryptable_balance,
                &authority,
                &[],
                &[&source_1, &source_2],
                proof_location,
            )
            .unwrap();

            check_no_panic(instruction.clone());

            let message = Message::new(&[instruction], None);
            let parsed = parse_token(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None),
            )
            .unwrap();

            // Core Property Assertions
            assert_eq!(
                parsed.instruction_type, "withdrawWithheldConfidentialTransferTokensFromAccounts",
                "Failed on: {name}",
            );
            assert_eq!(
                parsed.info["mint"],
                json!(mint.to_string()),
                "Failed on: {name}",
            );
            assert_eq!(
                parsed.info["feeRecipient"],
                json!(destination.to_string()),
                "Failed on: {name}",
            );
            assert_eq!(
                parsed.info["withdrawWithheldAuthority"],
                json!(authority.to_string()),
                "Failed on: {name}",
            );
            assert_eq!(
                parsed.info["sourceAccounts"],
                json!(vec![source_1.to_string(), source_2.to_string()]),
                "Failed on: {name}",
            );
            assert_eq!(
                parsed.info["newDecryptableAvailableBalance"],
                json!(format!("{new_decryptable_balance}")),
                "Failed on: {name}",
            );

            assert_eq!(
                parsed.info["proofInstructionOffset"],
                json!(expected_offset),
                "Failed on: {name}",
            );

            if expected_offset == 0 {
                assert_eq!(
                    parsed.info["proofContextStateAccount"],
                    json!(proof_ctx.to_string()),
                    "Proof Context mismatch on: {name}",
                );
                assert!(
                    parsed.info.get("instructionsSysvar").is_none(),
                    "Sysvar should not be present on: {name}",
                );
            } else {
                assert_eq!(
                    parsed.info["instructionsSysvar"],
                    json!(sysvar::instructions::id().to_string()),
                    "Sysvar mismatch on: {name}",
                );
                assert!(
                    parsed.info.get("proofContextStateAccount").is_none(),
                    "Proof Context should not be present on: {name}",
                );
            }
        }
    }
}
