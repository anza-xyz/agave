use {
    super::*,
    spl_token_2022_interface::{
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
            let offset = 1;

            if offset < account_indexes.len().saturating_sub(1) {
                if rotate_supply_data.proof_instruction_offset == 0 {
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
            let has_sysvar = mint_data.equality_proof_instruction_offset != 0
                || mint_data.ciphertext_validity_proof_instruction_offset != 0
                || mint_data.range_proof_instruction_offset != 0;

            if has_sysvar && offset < account_indexes.len().saturating_sub(1) {
                map.insert(
                    "instructionsSysvar".to_string(),
                    json!(account_keys[account_indexes[offset] as usize].to_string()),
                );
                offset += 1;
            }

            if mint_data.equality_proof_instruction_offset == 0
                && offset < account_indexes.len().saturating_sub(1)
            {
                map.insert(
                    "equalityProofContextStateAccount".to_string(),
                    json!(account_keys[account_indexes[offset] as usize].to_string()),
                );
                offset += 1;
            }
            if mint_data.ciphertext_validity_proof_instruction_offset == 0
                && offset < account_indexes.len().saturating_sub(1)
            {
                map.insert(
                    "ciphertextValidityProofContextStateAccount".to_string(),
                    json!(account_keys[account_indexes[offset] as usize].to_string()),
                );
                offset += 1;
            }
            if mint_data.range_proof_instruction_offset == 0
                && offset < account_indexes.len().saturating_sub(1)
            {
                map.insert(
                    "rangeProofContextStateAccount".to_string(),
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
            let has_sysvar = burn_data.equality_proof_instruction_offset != 0
                || burn_data.ciphertext_validity_proof_instruction_offset != 0
                || burn_data.range_proof_instruction_offset != 0;

            if has_sysvar && offset < account_indexes.len().saturating_sub(1) {
                map.insert(
                    "instructionsSysvar".to_string(),
                    json!(account_keys[account_indexes[offset] as usize].to_string()),
                );
                offset += 1;
            }

            if burn_data.equality_proof_instruction_offset == 0
                && offset < account_indexes.len().saturating_sub(1)
            {
                map.insert(
                    "equalityProofContextStateAccount".to_string(),
                    json!(account_keys[account_indexes[offset] as usize].to_string()),
                );
                offset += 1;
            }
            if burn_data.ciphertext_validity_proof_instruction_offset == 0
                && offset < account_indexes.len().saturating_sub(1)
            {
                map.insert(
                    "ciphertextValidityProofContextStateAccount".to_string(),
                    json!(account_keys[account_indexes[offset] as usize].to_string()),
                );
                offset += 1;
            }
            if burn_data.range_proof_instruction_offset == 0
                && offset < account_indexes.len().saturating_sub(1)
            {
                map.insert(
                    "rangeProofContextStateAccount".to_string(),
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
        solana_sdk_ids::sysvar,
        solana_zk_sdk_pod::encryption::{
            auth_encryption::PodAeCiphertext,
            elgamal::{PodElGamalCiphertext, PodElGamalPubkey},
        },
        spl_token_2022_interface::{
            extension::confidential_mint_burn::instruction::{
                confidential_burn_with_split_proofs, confidential_mint_with_split_proofs,
                initialize_mint,
            },
            solana_zk_elgamal_proof_interface::proof_data::{
                BatchedGroupedCiphertext3HandlesValidityProofData, BatchedRangeProofU128Data,
                CiphertextCiphertextEqualityProofData, CiphertextCommitmentEqualityProofData,
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
    fn test_initialize() {
        let instruction = initialize_mint(
            &spl_token_2022_interface::id(),
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
            &spl_token_2022_interface::id(),
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &[],
            &PodAeCiphertext::default(),
        )
        .unwrap();
        check_no_panic(instruction);
    }

    #[test]
    fn test_mint() {
        let destination = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let authority = Pubkey::new_unique();

        let equality_ctx = Pubkey::new_unique();
        let validity_ctx = Pubkey::new_unique();
        let range_ctx = Pubkey::new_unique();

        let offset_eq_1 = ProofLocation::InstructionOffset(
            NonZero::new(1).unwrap(),
            &CiphertextCommitmentEqualityProofData::zeroed(),
        );

        let offset_val_1 = ProofLocation::InstructionOffset(
            NonZero::new(1).unwrap(),
            &BatchedGroupedCiphertext3HandlesValidityProofData::zeroed(),
        );
        let offset_val_2 = ProofLocation::InstructionOffset(
            NonZero::new(2).unwrap(),
            &BatchedGroupedCiphertext3HandlesValidityProofData::zeroed(),
        );

        let offset_rng_2 = ProofLocation::InstructionOffset(
            NonZero::new(2).unwrap(),
            &BatchedRangeProofU128Data::zeroed(),
        );
        let offset_rng_3 = ProofLocation::InstructionOffset(
            NonZero::new(3).unwrap(),
            &BatchedRangeProofU128Data::zeroed(),
        );

        let context_eq = ProofLocation::ContextStateAccount(&equality_ctx);
        let context_val = ProofLocation::ContextStateAccount(&validity_ctx);
        let context_rng = ProofLocation::ContextStateAccount(&range_ctx);

        // We test exhaustive combinations to ensure the parser's offset logic holds.
        // Array of cases: (Test Name, Eq Proof, Val Proof, Rng Proof, Eq Offset, Val
        // Offset, Rng Offset)
        let cases = vec![
            (
                "All Contexts",
                context_eq,
                context_val,
                context_rng,
                0,
                0,
                0,
            ),
            (
                "All Offsets",
                offset_eq_1,
                offset_val_2,
                offset_rng_3,
                1,
                2,
                3,
            ),
            (
                "Mixed C-I-C",
                context_eq,
                offset_val_1,
                context_rng,
                0,
                1,
                0,
            ),
            (
                "Mixed I-C-I",
                offset_eq_1,
                context_val,
                offset_rng_2,
                1,
                0,
                2,
            ),
        ];

        for (name, eq, val, rng, eq_offset, val_offset, rng_offset) in cases {
            let instructions = confidential_mint_with_split_proofs(
                &spl_token_2022_interface::id(),
                &destination,
                &mint,
                &PodElGamalCiphertext::default(),
                &PodElGamalCiphertext::default(),
                &authority,
                &[],
                eq,
                val,
                rng,
                &PodAeCiphertext::default(),
            )
            .unwrap();

            let message = Message::new(&instructions, None);
            let parsed = parse_token(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None),
            )
            .unwrap();

            // Core Property Assertions
            assert_eq!(
                parsed.instruction_type, "confidentialMint",
                "Failed on: {name}",
            );
            assert_eq!(
                parsed.info["destination"],
                json!(destination.to_string()),
                "Failed on: {name}",
            );
            assert_eq!(
                parsed.info["mint"],
                json!(mint.to_string()),
                "Failed on: {name}",
            );
            assert_eq!(
                parsed.info["owner"],
                json!(authority.to_string()),
                "Failed on: {name}",
            );
            assert_eq!(
                parsed.info["newDecryptableSupply"],
                json!(format!("{}", PodAeCiphertext::default())),
                "Failed on: {name}",
            );

            // Expected Offsets
            assert_eq!(
                parsed.info["equalityProofInstructionOffset"],
                json!(eq_offset),
                "Failed on: {name}",
            );
            assert_eq!(
                parsed.info["ciphertextValidityProofInstructionOffset"],
                json!(val_offset),
                "Failed on: {name}",
            );
            assert_eq!(
                parsed.info["rangeProofInstructionOffset"],
                json!(rng_offset),
                "Failed on: {name}",
            );

            // Sysvar Assertion: Only present if at least one proof relies on an
            // instruction offset
            if eq_offset != 0 || val_offset != 0 || rng_offset != 0 {
                assert_eq!(
                    parsed.info["instructionsSysvar"],
                    json!(sysvar::instructions::id().to_string()),
                    "Sysvar mismatch on: {name}",
                );
            } else {
                assert!(
                    parsed.info.get("instructionsSysvar").is_none(),
                    "Sysvar mismatch on: {name}",
                );
            }

            // Eq Context State Account
            if eq_offset == 0 {
                assert_eq!(
                    parsed.info["equalityProofContextStateAccount"],
                    json!(equality_ctx.to_string()),
                    "Eq Context mismatch on: {name}",
                );
            } else {
                assert!(
                    parsed
                        .info
                        .get("equalityProofContextStateAccount")
                        .is_none(),
                    "Eq Context mismatch on: {name}",
                );
            }

            // Val Context State Account
            if val_offset == 0 {
                assert_eq!(
                    parsed.info["ciphertextValidityProofContextStateAccount"],
                    json!(validity_ctx.to_string()),
                    "Val Context mismatch on: {name}",
                );
            } else {
                assert!(
                    parsed
                        .info
                        .get("ciphertextValidityProofContextStateAccount")
                        .is_none(),
                    "Val Context mismatch on: {name}",
                );
            }

            // Rng Context State Account
            if rng_offset == 0 {
                assert_eq!(
                    parsed.info["rangeProofContextStateAccount"],
                    json!(range_ctx.to_string()),
                    "Rng Context mismatch on: {name}",
                );
            } else {
                assert!(
                    parsed.info.get("rangeProofContextStateAccount").is_none(),
                    "Rng Context mismatch on: {name}",
                );
            }
        }
    }

    #[test]
    fn test_burn() {
        let destination = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let authority = Pubkey::new_unique();

        let equality_ctx = Pubkey::new_unique();
        let validity_ctx = Pubkey::new_unique();
        let range_ctx = Pubkey::new_unique();

        let offset_eq_1 = ProofLocation::InstructionOffset(
            NonZero::new(1).unwrap(),
            &CiphertextCommitmentEqualityProofData::zeroed(),
        );

        let offset_val_1 = ProofLocation::InstructionOffset(
            NonZero::new(1).unwrap(),
            &BatchedGroupedCiphertext3HandlesValidityProofData::zeroed(),
        );
        let offset_val_2 = ProofLocation::InstructionOffset(
            NonZero::new(2).unwrap(),
            &BatchedGroupedCiphertext3HandlesValidityProofData::zeroed(),
        );

        let offset_rng_2 = ProofLocation::InstructionOffset(
            NonZero::new(2).unwrap(),
            &BatchedRangeProofU128Data::zeroed(),
        );
        let offset_rng_3 = ProofLocation::InstructionOffset(
            NonZero::new(3).unwrap(),
            &BatchedRangeProofU128Data::zeroed(),
        );

        let context_eq = ProofLocation::ContextStateAccount(&equality_ctx);
        let context_val = ProofLocation::ContextStateAccount(&validity_ctx);
        let context_rng = ProofLocation::ContextStateAccount(&range_ctx);

        // We test exhaustive combinations to ensure the parser's offset logic holds.
        // Array of cases: (Test Name, Eq Proof, Val Proof, Rng Proof, Eq Offset, Val
        // Offset, Rng Offset)
        let cases = vec![
            (
                "All Contexts",
                context_eq,
                context_val,
                context_rng,
                0,
                0,
                0,
            ),
            (
                "All Offsets",
                offset_eq_1,
                offset_val_2,
                offset_rng_3,
                1,
                2,
                3,
            ),
            (
                "Mixed C-I-C",
                context_eq,
                offset_val_1,
                context_rng,
                0,
                1,
                0,
            ),
            (
                "Mixed I-C-I",
                offset_eq_1,
                context_val,
                offset_rng_2,
                1,
                0,
                2,
            ),
        ];

        for (name, eq, val, rng, eq_offset, val_offset, rng_offset) in cases {
            let instructions = confidential_burn_with_split_proofs(
                &spl_token_2022_interface::id(),
                &destination,
                &mint,
                &PodAeCiphertext::default(),
                &PodElGamalCiphertext::default(),
                &PodElGamalCiphertext::default(),
                &authority,
                &[],
                eq,
                val,
                rng,
            )
            .unwrap();

            let message = Message::new(&instructions, None);
            let parsed = parse_token(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None),
            )
            .unwrap();

            // Core Property Assertions
            assert_eq!(
                parsed.instruction_type, "confidentialBurn",
                "Failed on: {name}",
            );
            assert_eq!(
                parsed.info["destination"],
                json!(destination.to_string()),
                "Failed on: {name}",
            );
            assert_eq!(
                parsed.info["mint"],
                json!(mint.to_string()),
                "Failed on: {name}",
            );
            assert_eq!(
                parsed.info["owner"],
                json!(authority.to_string()),
                "Failed on: {name}",
            );
            assert_eq!(
                parsed.info["newDecryptableAvailableBalance"],
                json!(format!("{}", PodAeCiphertext::default())),
                "Failed on: {name}",
            );

            // Expected Offsets
            assert_eq!(
                parsed.info["equalityProofInstructionOffset"],
                json!(eq_offset),
                "Failed on: {name}",
            );
            assert_eq!(
                parsed.info["ciphertextValidityProofInstructionOffset"],
                json!(val_offset),
                "Failed on: {name}",
            );
            assert_eq!(
                parsed.info["rangeProofInstructionOffset"],
                json!(rng_offset),
                "Failed on: {name}",
            );

            // Sysvar Assertion: Only present if at least one proof relies on an
            // instruction offset
            if eq_offset != 0 || val_offset != 0 || rng_offset != 0 {
                assert_eq!(
                    parsed.info["instructionsSysvar"],
                    json!(sysvar::instructions::id().to_string()),
                    "Sysvar mismatch on: {name}",
                );
            } else {
                assert!(
                    parsed.info.get("instructionsSysvar").is_none(),
                    "Sysvar mismatch on: {name}",
                );
            }

            // Eq Context State Account
            if eq_offset == 0 {
                assert_eq!(
                    parsed.info["equalityProofContextStateAccount"],
                    json!(equality_ctx.to_string()),
                    "Eq Context mismatch on: {name}",
                );
            } else {
                assert!(
                    parsed
                        .info
                        .get("equalityProofContextStateAccount")
                        .is_none(),
                    "Eq Context mismatch on: {name}",
                );
            }

            // Val Context State Account
            if val_offset == 0 {
                assert_eq!(
                    parsed.info["ciphertextValidityProofContextStateAccount"],
                    json!(validity_ctx.to_string()),
                    "Val Context mismatch on: {name}",
                );
            } else {
                assert!(
                    parsed
                        .info
                        .get("ciphertextValidityProofContextStateAccount")
                        .is_none(),
                    "Val Context mismatch on: {name}",
                );
            }

            // Rng Context State Account
            if rng_offset == 0 {
                assert_eq!(
                    parsed.info["rangeProofContextStateAccount"],
                    json!(range_ctx.to_string()),
                    "Rng Context mismatch on: {name}",
                );
            } else {
                assert!(
                    parsed.info.get("rangeProofContextStateAccount").is_none(),
                    "Rng Context mismatch on: {name}",
                );
            }
        }
    }

    #[test]
    fn test_rotate() {
        let mint = Pubkey::new_unique();
        let authority = Pubkey::new_unique();
        let proof_ctx = Pubkey::new_unique();

        let offset_proof = ProofLocation::InstructionOffset(
            NonZero::new(1).unwrap(),
            &CiphertextCiphertextEqualityProofData::zeroed(),
        );
        let context_proof = ProofLocation::ContextStateAccount(&proof_ctx);

        let cases = vec![
            ("Context State Account", context_proof, 0),
            ("Instruction Offset", offset_proof, 1),
        ];

        for (name, proof_location, expected_offset) in cases {
            let instructions = rotate_supply_elgamal_pubkey(
                &spl_token_2022_interface::id(),
                &mint,
                &authority,
                &[],
                &PodElGamalPubkey::default(),
                proof_location,
            )
            .unwrap();

            check_no_panic(instructions[0].clone());

            let message = Message::new(&instructions, None);
            let parsed = parse_token(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None),
            )
            .unwrap();

            // Core Property Assertions
            assert_eq!(
                parsed.instruction_type, "rotateConfidentialMintBurnSupplyElGamalPubkey",
                "Failed on: {name}"
            );
            assert_eq!(
                parsed.info["mint"],
                json!(mint.to_string()),
                "Failed on: {name}"
            );
            assert_eq!(
                parsed.info["newSupplyElGamalPubkey"],
                json!(PodElGamalPubkey::default().to_string()),
                "Failed on: {name}"
            );
            assert_eq!(
                parsed.info["proofInstructionOffset"],
                json!(expected_offset),
                "Failed on: {name}"
            );

            // Conditional Proof Context / Sysvar Assertions
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

            assert_eq!(
                parsed.info["owner"],
                json!(authority.to_string()),
                "Failed on: {name}",
            );
        }
    }
}
