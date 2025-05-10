use {
    super::*,
    spl_token_2022::{
        extension::confidential_mint_burn::instruction::*,
        instruction::{decode_instruction_data, decode_instruction_type},
    },
};

pub(in crate::parse_token) fn parse_confidential_mint_burn_instruction(
    instruction_data: &[u8],
    account_indexes: &[u8],
    account_keys: &AccountKeys,
) -> Result<ParsedInstructionEnum, ParseInstructionError> {
    match decode_instruction_type(instruction_data)
        .map_err(|_| ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken))?
    {
        ConfidentialMintBurnInstruction::InitializeMint => {
            check_num_token_accounts(account_indexes, 1)?;
            let initialize_mint_data: InitializeMintData =
                *decode_instruction_data(instruction_data).map_err(|_| {
                    ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken)
                })?;
            let value = json!({
                "mint": account_keys[account_indexes[0] as usize].to_string(),
                "supplyElGamalPubkey": initialize_mint_data.supply_elgamal_pubkey.to_string(),
                "decryptableSupply": initialize_mint_data.decryptable_supply.to_string(),
            });
            Ok(ParsedInstructionEnum {
                instruction_type: "initializeConfidentialMintBurnMint".to_string(),
                info: value,
            })
        }
        ConfidentialMintBurnInstruction::UpdateDecryptableSupply => {
            check_num_token_accounts(account_indexes, 2)?;
            let update_decryptable_supply: UpdateDecryptableSupplyData =
                *decode_instruction_data(instruction_data).map_err(|_| {
                    ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken)
                })?;
            let mut value = json!({
                "mint": account_keys[account_indexes[0] as usize].to_string(),
                "newDecryptableSupply": update_decryptable_supply.new_decryptable_supply.to_string(),

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
                instruction_type: "updateConfidentialMintBurnDecryptableSupply".to_string(),
                info: value,
            })
        }
        ConfidentialMintBurnInstruction::RotateSupplyElGamalPubkey => {
            check_num_token_accounts(account_indexes, 3)?;
            let rotate_supply_data: RotateSupplyElGamalPubkeyData =
                *decode_instruction_data(instruction_data).map_err(|_| {
                    ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken)
                })?;
            let mut value = json!({
                "mint": account_keys[account_indexes[0] as usize].to_string(),
                "newSupplyElGamalPubkey": rotate_supply_data.new_supply_elgamal_pubkey.to_string(),
                "proofInstructionOffset": rotate_supply_data.proof_instruction_offset,

            });
            let map = value.as_object_mut().unwrap();
            if rotate_supply_data.proof_instruction_offset == 0 {
                map.insert(
                    "proofAccount".to_string(),
                    json!(account_keys[account_indexes[1] as usize].to_string()),
                );
            } else {
                map.insert(
                    "instructionsSysvar".to_string(),
                    json!(account_keys[account_indexes[1] as usize].to_string()),
                );
            }
            parse_signers(
                map,
                2,
                account_keys,
                account_indexes,
                "owner",
                "multisigOwner",
            );
            Ok(ParsedInstructionEnum {
                instruction_type: "rotateConfidentialMintBurnSupplyElGamalPubkey".to_string(),
                info: value,
            })
        }
        ConfidentialMintBurnInstruction::Mint => {
            check_num_token_accounts(account_indexes, 3)?;
            let mint_data: MintInstructionData = *decode_instruction_data(instruction_data)
                .map_err(|_| {
                    ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken)
                })?;
            let mut value = json!({
                "destination": account_keys[account_indexes[0] as usize].to_string(),
                "mint": account_keys[account_indexes[1] as usize].to_string(),
                "newDecryptableSupply": mint_data.new_decryptable_supply.to_string(),
                "equalityProofInstructionOffset": mint_data.equality_proof_instruction_offset,
                "ciphertextValidityProofInstructionOffset": mint_data.ciphertext_validity_proof_instruction_offset,
                "rangeProofInstructionOffset": mint_data.range_proof_instruction_offset,

            });
            let mut offset = 2;
            let map = value.as_object_mut().unwrap();
            if offset < account_indexes.len() - 1
                && (mint_data.equality_proof_instruction_offset != 0
                    || mint_data.ciphertext_validity_proof_instruction_offset != 0
                    || mint_data.range_proof_instruction_offset != 0)
            {
                map.insert(
                    "instructionsSysvar".to_string(),
                    json!(account_keys[account_indexes[offset] as usize].to_string()),
                );
                offset += 1;
            }

            // Assume that extra accounts are proof accounts and not multisig
            // signers. This might be wrong, but it's the best possible option.
            if offset < account_indexes.len() - 1 {
                let label = if mint_data.equality_proof_instruction_offset == 0 {
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
            if offset < account_indexes.len() - 1 {
                let label = if mint_data.ciphertext_validity_proof_instruction_offset == 0 {
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
            if offset < account_indexes.len() - 1 {
                let label = if mint_data.range_proof_instruction_offset == 0 {
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

            parse_signers(
                map,
                offset,
                account_keys,
                account_indexes,
                "owner",
                "multisigOwner",
            );
            Ok(ParsedInstructionEnum {
                instruction_type: "confidentialMint".to_string(),
                info: value,
            })
        }
        ConfidentialMintBurnInstruction::Burn => {
            check_num_token_accounts(account_indexes, 3)?;
            let burn_data: BurnInstructionData = *decode_instruction_data(instruction_data)
                .map_err(|_| {
                    ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken)
                })?;
            let mut value = json!({
                "destination": account_keys[account_indexes[0] as usize].to_string(),
                "mint": account_keys[account_indexes[1] as usize].to_string(),
                "newDecryptableAvailableBalance": burn_data.new_decryptable_available_balance.to_string(),
                "equalityProofInstructionOffset": burn_data.equality_proof_instruction_offset,
                "ciphertextValidityProofInstructionOffset": burn_data.ciphertext_validity_proof_instruction_offset,
                "rangeProofInstructionOffset": burn_data.range_proof_instruction_offset,

            });
            let mut offset = 2;
            let map = value.as_object_mut().unwrap();
            if offset < account_indexes.len() - 1
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
            if offset < account_indexes.len() - 1 {
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
            if offset < account_indexes.len() - 1 {
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
            if offset < account_indexes.len() - 1 {
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

            parse_signers(
                map,
                offset,
                account_keys,
                account_indexes,
                "owner",
                "multisigOwner",
            );
            Ok(ParsedInstructionEnum {
                instruction_type: "confidentialBurn".to_string(),
                info: value,
            })
        }
        ConfidentialMintBurnInstruction::ApplyPendingBurn => {
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
                "owner",
                "multisigOwner",
            );
            Ok(ParsedInstructionEnum {
                instruction_type: "applyPendingBurn".to_string(),
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
        spl_token_2022::{
            extension::confidential_mint_burn::instruction::{
                confidential_burn_with_split_proofs, confidential_mint_with_split_proofs,
                initialize_mint,
            },
            solana_zk_sdk::{
                encryption::pod::{
                    auth_encryption::PodAeCiphertext,
                    elgamal::{PodElGamalCiphertext, PodElGamalPubkey},
                },
                zk_elgamal_proof_program::proof_data::{
                    BatchedGroupedCiphertext3HandlesValidityProofData, BatchedRangeProofU128Data,
                    CiphertextCiphertextEqualityProofData, CiphertextCommitmentEqualityProofData,
                },
            },
        },
        spl_token_confidential_transfer_proof_extraction::instruction::{ProofData, ProofLocation},
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
    fn test_initialize() {
        let instruction = initialize_mint(
            &spl_token_2022::id(),
            &Pubkey::new_unique(),
            &PodElGamalPubkey::default(),
            &PodAeCiphertext::default(),
        )
        .unwrap();
        check_no_panic(instruction);
    }

    #[test]
    fn test_update() {
        let instruction = update_decryptable_supply(
            &spl_token_2022::id(),
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &[],
            &PodAeCiphertext::default(),
        )
        .unwrap();
        check_no_panic(instruction);
    }

    #[test]
    fn test_rotate() {
        for location in [
            ProofLocation::InstructionOffset(
                NonZero::new(1).unwrap(),
                ProofData::InstructionData(&CiphertextCiphertextEqualityProofData::zeroed()),
            ),
            ProofLocation::InstructionOffset(
                NonZero::new(1).unwrap(),
                ProofData::RecordAccount(&Pubkey::new_unique(), 0),
            ),
            ProofLocation::ContextStateAccount(&Pubkey::new_unique()),
        ] {
            let instructions = rotate_supply_elgamal_pubkey(
                &spl_token_2022::id(),
                &Pubkey::new_unique(),
                &Pubkey::new_unique(),
                &[],
                &PodElGamalPubkey::default(),
                location,
            )
            .unwrap();
            check_no_panic(instructions[0].clone());
        }
    }

    #[test]
    fn test_mint() {
        for (equality_proof_location, ciphertext_validity_proof_location, range_proof_location) in [
            (
                ProofLocation::InstructionOffset(
                    NonZero::new(1).unwrap(),
                    ProofData::InstructionData(&CiphertextCommitmentEqualityProofData::zeroed()),
                ),
                ProofLocation::InstructionOffset(
                    NonZero::new(2).unwrap(),
                    ProofData::InstructionData(
                        &BatchedGroupedCiphertext3HandlesValidityProofData::zeroed(),
                    ),
                ),
                ProofLocation::InstructionOffset(
                    NonZero::new(3).unwrap(),
                    ProofData::InstructionData(&BatchedRangeProofU128Data::zeroed()),
                ),
            ),
            (
                ProofLocation::InstructionOffset(
                    NonZero::new(1).unwrap(),
                    ProofData::RecordAccount(&Pubkey::new_unique(), 0),
                ),
                ProofLocation::InstructionOffset(
                    NonZero::new(2).unwrap(),
                    ProofData::RecordAccount(&Pubkey::new_unique(), 0),
                ),
                ProofLocation::InstructionOffset(
                    NonZero::new(3).unwrap(),
                    ProofData::RecordAccount(&Pubkey::new_unique(), 0),
                ),
            ),
            (
                ProofLocation::ContextStateAccount(&Pubkey::new_unique()),
                ProofLocation::ContextStateAccount(&Pubkey::new_unique()),
                ProofLocation::ContextStateAccount(&Pubkey::new_unique()),
            ),
        ] {
            let instructions = confidential_mint_with_split_proofs(
                &spl_token_2022::id(),
                &Pubkey::new_unique(),
                &Pubkey::new_unique(),
                &PodElGamalCiphertext::default(),
                &PodElGamalCiphertext::default(),
                &Pubkey::new_unique(),
                &[],
                equality_proof_location,
                ciphertext_validity_proof_location,
                range_proof_location,
                &PodAeCiphertext::default(),
            )
            .unwrap();
            check_no_panic(instructions[0].clone());
        }
    }

    #[test]
    fn test_burn() {
        for (equality_proof_location, ciphertext_validity_proof_location, range_proof_location) in [
            (
                ProofLocation::InstructionOffset(
                    NonZero::new(1).unwrap(),
                    ProofData::InstructionData(&CiphertextCommitmentEqualityProofData::zeroed()),
                ),
                ProofLocation::InstructionOffset(
                    NonZero::new(2).unwrap(),
                    ProofData::InstructionData(
                        &BatchedGroupedCiphertext3HandlesValidityProofData::zeroed(),
                    ),
                ),
                ProofLocation::InstructionOffset(
                    NonZero::new(3).unwrap(),
                    ProofData::InstructionData(&BatchedRangeProofU128Data::zeroed()),
                ),
            ),
            (
                ProofLocation::InstructionOffset(
                    NonZero::new(1).unwrap(),
                    ProofData::RecordAccount(&Pubkey::new_unique(), 0),
                ),
                ProofLocation::InstructionOffset(
                    NonZero::new(2).unwrap(),
                    ProofData::RecordAccount(&Pubkey::new_unique(), 0),
                ),
                ProofLocation::InstructionOffset(
                    NonZero::new(3).unwrap(),
                    ProofData::RecordAccount(&Pubkey::new_unique(), 0),
                ),
            ),
            (
                ProofLocation::ContextStateAccount(&Pubkey::new_unique()),
                ProofLocation::ContextStateAccount(&Pubkey::new_unique()),
                ProofLocation::ContextStateAccount(&Pubkey::new_unique()),
            ),
        ] {
            let instructions = confidential_burn_with_split_proofs(
                &spl_token_2022::id(),
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
