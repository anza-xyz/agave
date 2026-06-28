use {
    super::*,
    spl_token_2022_interface::{
        extension::{
            confidential_mint_burn::instruction::BurnInstructionData as ConfidentialBurnInstructionData,
            permissioned_burn::instruction::{
                BurnCheckedInstructionData, BurnInstructionData, InitializeInstructionData,
                PermissionedBurnInstruction,
            },
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
        PermissionedBurnInstruction::ConfidentialBurn => {
            check_num_token_accounts(account_indexes, 4)?;
            let burn_data: ConfidentialBurnInstructionData =
                *decode_instruction_data(instruction_data).map_err(|_| {
                    ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken)
                })?;
            let mut value = json!({
                "account": account_keys[account_indexes[0] as usize].to_string(),
                "mint": account_keys[account_indexes[1] as usize].to_string(),
                "newDecryptableAvailableBalance": burn_data.new_decryptable_available_balance.to_string(),
                "equalityProofInstructionOffset": burn_data.equality_proof_instruction_offset,
                "ciphertextValidityProofInstructionOffset": burn_data.ciphertext_validity_proof_instruction_offset,
                "rangeProofInstructionOffset": burn_data.range_proof_instruction_offset,
            });
            let map = value.as_object_mut().unwrap();
            // The permissioned burn authority and the owner/delegate are the
            // trailing accounts; everything between the mint and them is optional
            // proof material. Reserve those two when walking the proof accounts.
            let mut offset = 2;
            if offset < account_indexes.len() - 2
                && (burn_data.equality_proof_instruction_offset != 0
                    || burn_data.ciphertext_validity_proof_instruction_offset != 0
                    || burn_data.range_proof_instruction_offset != 0)
            {
                map.insert(
                    "instructionsSysvar".to_string(),
                    json!(account_keys[account_indexes[offset] as usize].to_string()),
                );
                offset += 1;
            }
            // Assume that extra accounts are proof accounts and not multisig
            // signers. This might be wrong, but it's the best possible option.
            if offset < account_indexes.len() - 2 {
                let label = if burn_data.equality_proof_instruction_offset == 0 {
                    "equalityProofContextStateAccount"
                } else {
                    "equalityProofRecordAccount"
                };
                map.insert(
                    label.to_string(),
                    json!(account_keys[account_indexes[offset] as usize].to_string()),
                );
                offset += 1;
            }
            if offset < account_indexes.len() - 2 {
                let label = if burn_data.ciphertext_validity_proof_instruction_offset == 0 {
                    "ciphertextValidityProofContextStateAccount"
                } else {
                    "ciphertextValidityProofRecordAccount"
                };
                map.insert(
                    label.to_string(),
                    json!(account_keys[account_indexes[offset] as usize].to_string()),
                );
                offset += 1;
            }
            if offset < account_indexes.len() - 2 {
                let label = if burn_data.range_proof_instruction_offset == 0 {
                    "rangeProofContextStateAccount"
                } else {
                    "rangeProofRecordAccount"
                };
                map.insert(
                    label.to_string(),
                    json!(account_keys[account_indexes[offset] as usize].to_string()),
                );
                offset += 1;
            }
            map.insert(
                "permissionedBurnAuthority".to_string(),
                json!(account_keys[account_indexes[offset] as usize].to_string()),
            );
            offset += 1;
            parse_signers(
                map,
                offset,
                account_keys,
                account_indexes,
                "authority",
                "multisigAuthority",
            );
            Ok(ParsedInstructionEnum {
                instruction_type: "permissionedConfidentialBurn".to_string(),
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
        solana_zk_sdk_pod::encryption::{
            auth_encryption::PodAeCiphertext, elgamal::PodElGamalCiphertext,
        },
        spl_token_2022_interface::{
            extension::permissioned_burn::instruction::{
                burn, confidential_burn_with_split_proofs, initialize,
            },
            solana_zk_elgamal_proof_interface::proof_data::{
                BatchedGroupedCiphertext3HandlesValidityProofData, BatchedRangeProofU128Data,
                CiphertextCommitmentEqualityProofData,
            },
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

    #[test]
    fn test_parse_permissioned_confidential_burn_instruction() {
        for (equality_proof_location, ciphertext_validity_proof_location, range_proof_location) in [
            (
                ProofLocation::InstructionOffset(
                    NonZero::new(1).unwrap(),
                    &CiphertextCommitmentEqualityProofData::zeroed(),
                ),
                ProofLocation::InstructionOffset(
                    NonZero::new(2).unwrap(),
                    &BatchedGroupedCiphertext3HandlesValidityProofData::zeroed(),
                ),
                ProofLocation::InstructionOffset(
                    NonZero::new(3).unwrap(),
                    &BatchedRangeProofU128Data::zeroed(),
                ),
            ),
            (
                ProofLocation::ContextStateAccount(&Pubkey::new_unique()),
                ProofLocation::ContextStateAccount(&Pubkey::new_unique()),
                ProofLocation::ContextStateAccount(&Pubkey::new_unique()),
            ),
        ] {
            let instructions = confidential_burn_with_split_proofs(
                &spl_token_2022_interface::id(),
                &Pubkey::new_unique(),
                &Pubkey::new_unique(),
                &Pubkey::new_unique(),
                &PodAeCiphertext::default(),
                &PodElGamalCiphertext::default(),
                &PodElGamalCiphertext::default(),
                &Pubkey::new_unique(),
                &[],
                equality_proof_location,
                ciphertext_validity_proof_location,
                range_proof_location,
            )
            .unwrap();
            check_no_panic(instructions[0].clone());
        }
    }
}
