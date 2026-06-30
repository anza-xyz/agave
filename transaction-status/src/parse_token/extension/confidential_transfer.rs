use {
    super::*,
    solana_zk_sdk_pod::encryption::elgamal::PodElGamalPubkey,
    spl_token_2022_interface::{
        extension::confidential_transfer::instruction::*,
        instruction::{decode_instruction_data, decode_instruction_type},
    },
};

pub(in crate::parse_token) fn parse_confidential_transfer_instruction(
    instruction_data: &[u8],
    account_indexes: &[u8],
    account_keys: &AccountKeys,
) -> Result<ParsedInstructionEnum, ParseInstructionError> {
    match decode_instruction_type(instruction_data)
        .map_err(|_| ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken))?
    {
        ConfidentialTransferInstruction::InitializeMint => {
            check_num_token_accounts(account_indexes, 1)?;
            let initialize_mint_data: InitializeMintData =
                *decode_instruction_data(instruction_data).map_err(|_| {
                    ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken)
                })?;
            let mut value = json!({
                "mint": account_keys[account_indexes[0] as usize].to_string(),
                "autoApproveNewAccounts": bool::from(initialize_mint_data.auto_approve_new_accounts),
                "auditorElGamalPubkey": Option::<PodElGamalPubkey>::from(initialize_mint_data.auditor_elgamal_pubkey).map(|k| k.to_string()),
            });
            let map = value.as_object_mut().unwrap();
            if let Some(authority) = Option::<Pubkey>::from(initialize_mint_data.authority) {
                map.insert("authority".to_string(), json!(authority.to_string()));
            }
            Ok(ParsedInstructionEnum {
                instruction_type: "initializeConfidentialTransferMint".to_string(),
                info: value,
            })
        }
        ConfidentialTransferInstruction::UpdateMint => {
            check_num_token_accounts(account_indexes, 2)?;
            let update_mint_data: UpdateMintData = *decode_instruction_data(instruction_data)
                .map_err(|_| {
                    ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken)
                })?;
            let value = json!({
                "mint": account_keys[account_indexes[0] as usize].to_string(),
                "confidentialTransferMintAuthority": account_keys[account_indexes[1] as usize].to_string(),
                "autoApproveNewAccounts": bool::from(update_mint_data.auto_approve_new_accounts),
                "auditorElGamalPubkey": Option::<PodElGamalPubkey>::from(update_mint_data.auditor_elgamal_pubkey).map(|k| k.to_string()),
            });
            Ok(ParsedInstructionEnum {
                instruction_type: "updateConfidentialTransferMint".to_string(),
                info: value,
            })
        }
        ConfidentialTransferInstruction::ConfigureAccount => {
            check_num_token_accounts(account_indexes, 4)?;
            let configure_account_data: ConfigureAccountInstructionData =
                *decode_instruction_data(instruction_data).map_err(|_| {
                    ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken)
                })?;
            let maximum_pending_balance_credit_counter: u64 = configure_account_data
                .maximum_pending_balance_credit_counter
                .into();
            let mut value = json!({
                "account": account_keys[account_indexes[0] as usize].to_string(),
                "mint": account_keys[account_indexes[1] as usize].to_string(),
                "decryptableZeroBalance": format!("{}", configure_account_data.decryptable_zero_balance),
                "maximumPendingBalanceCreditCounter": maximum_pending_balance_credit_counter,
                "proofInstructionOffset": configure_account_data.proof_instruction_offset,

            });
            let map = value.as_object_mut().unwrap();

            if configure_account_data.proof_instruction_offset == 0 {
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

            parse_signers(
                map,
                3,
                account_keys,
                account_indexes,
                "owner",
                "multisigOwner",
            );
            Ok(ParsedInstructionEnum {
                instruction_type: "configureConfidentialTransferAccount".to_string(),
                info: value,
            })
        }
        ConfidentialTransferInstruction::ApproveAccount => {
            check_num_token_accounts(account_indexes, 3)?;
            Ok(ParsedInstructionEnum {
                instruction_type: "approveConfidentialTransferAccount".to_string(),
                info: json!({
                    "account": account_keys[account_indexes[0] as usize].to_string(),
                    "mint": account_keys[account_indexes[1] as usize].to_string(),
                    "confidentialTransferAuditorAuthority": account_keys[account_indexes[2] as usize].to_string(),
                }),
            })
        }
        ConfidentialTransferInstruction::EmptyAccount => {
            check_num_token_accounts(account_indexes, 3)?;
            let empty_account_data: EmptyAccountInstructionData =
                *decode_instruction_data(instruction_data).map_err(|_| {
                    ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken)
                })?;
            let proof_instruction_offset: i8 = empty_account_data.proof_instruction_offset;
            let mut value = json!({
                "account": account_keys[account_indexes[0] as usize].to_string(),
                "proofInstructionOffset": proof_instruction_offset,

            });
            let map = value.as_object_mut().unwrap();

            if proof_instruction_offset == 0 {
                map.insert(
                    "proofContextStateAccount".to_string(),
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
                instruction_type: "emptyConfidentialTransferAccount".to_string(),
                info: value,
            })
        }
        ConfidentialTransferInstruction::Deposit => {
            check_num_token_accounts(account_indexes, 3)?;
            let deposit_data: DepositInstructionData = *decode_instruction_data(instruction_data)
                .map_err(|_| {
                ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken)
            })?;
            let amount: u64 = deposit_data.amount.into();
            let mut value = json!({
                "account": account_keys[account_indexes[0] as usize].to_string(),
                "mint": account_keys[account_indexes[1] as usize].to_string(),
                "amount": amount,
                "decimals": deposit_data.decimals,

            });
            let map = value.as_object_mut().unwrap();
            parse_signers(
                map,
                2,
                account_keys,
                account_indexes,
                "owner",
                "multisigOwner",
            );
            Ok(ParsedInstructionEnum {
                instruction_type: "depositConfidentialTransfer".to_string(),
                info: value,
            })
        }
        ConfidentialTransferInstruction::Withdraw => {
            check_num_token_accounts(account_indexes, 4)?;
            let withdrawal_data: WithdrawInstructionData =
                *decode_instruction_data(instruction_data).map_err(|_| {
                    ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken)
                })?;
            let amount: u64 = withdrawal_data.amount.into();
            let mut value = json!({
                "account": account_keys[account_indexes[0] as usize].to_string(),
                "mint": account_keys[account_indexes[1] as usize].to_string(),
                "amount": amount,
                "decimals": withdrawal_data.decimals,
                "newDecryptableAvailableBalance": format!("{}", withdrawal_data.new_decryptable_available_balance),
                "equalityProofInstructionOffset": withdrawal_data.equality_proof_instruction_offset,
                "rangeProofInstructionOffset": withdrawal_data.range_proof_instruction_offset,

            });

            let mut offset = 2;
            let map = value.as_object_mut().unwrap();
            let has_sysvar = withdrawal_data.equality_proof_instruction_offset != 0
                || withdrawal_data.range_proof_instruction_offset != 0;

            if has_sysvar && offset < account_indexes.len().saturating_sub(1) {
                map.insert(
                    "instructionsSysvar".to_string(),
                    json!(account_keys[account_indexes[offset] as usize].to_string()),
                );
                offset += 1;
            }

            if withdrawal_data.equality_proof_instruction_offset == 0
                && offset < account_indexes.len().saturating_sub(1)
            {
                map.insert(
                    "equalityProofContextStateAccount".to_string(),
                    json!(account_keys[account_indexes[offset] as usize].to_string()),
                );
                offset += 1;
            }

            if withdrawal_data.range_proof_instruction_offset == 0
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
                instruction_type: "withdrawConfidentialTransfer".to_string(),
                info: value,
            })
        }
        ConfidentialTransferInstruction::Transfer => {
            check_num_token_accounts(account_indexes, 4)?;
            let transfer_data: TransferInstructionData = *decode_instruction_data(instruction_data)
                .map_err(|_| {
                    ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken)
                })?;
            let mut value = json!({
                "source": account_keys[account_indexes[0] as usize].to_string(),
                "mint": account_keys[account_indexes[1] as usize].to_string(),
                "destination": account_keys[account_indexes[2] as usize].to_string(),
                "newSourceDecryptableAvailableBalance": format!("{}", transfer_data.new_source_decryptable_available_balance),
                "equalityProofInstructionOffset": transfer_data.equality_proof_instruction_offset,
                "ciphertextValidityProofInstructionOffset": transfer_data.ciphertext_validity_proof_instruction_offset,
                "rangeProofInstructionOffset": transfer_data.range_proof_instruction_offset,

            });
            let mut offset = 3;
            let map = value.as_object_mut().unwrap();
            let has_sysvar = transfer_data.equality_proof_instruction_offset != 0
                || transfer_data.ciphertext_validity_proof_instruction_offset != 0
                || transfer_data.range_proof_instruction_offset != 0;

            if has_sysvar && offset < account_indexes.len().saturating_sub(1) {
                map.insert(
                    "instructionsSysvar".to_string(),
                    json!(account_keys[account_indexes[offset] as usize].to_string()),
                );
                offset += 1;
            }

            if transfer_data.equality_proof_instruction_offset == 0
                && offset < account_indexes.len().saturating_sub(1)
            {
                map.insert(
                    "equalityProofContextStateAccount".to_string(),
                    json!(account_keys[account_indexes[offset] as usize].to_string()),
                );
                offset += 1;
            }

            if transfer_data.ciphertext_validity_proof_instruction_offset == 0
                && offset < account_indexes.len().saturating_sub(1)
            {
                map.insert(
                    "ciphertextValidityProofContextStateAccount".to_string(),
                    json!(account_keys[account_indexes[offset] as usize].to_string()),
                );
                offset += 1;
            }

            if transfer_data.range_proof_instruction_offset == 0
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
                instruction_type: "confidentialTransfer".to_string(),
                info: value,
            })
        }
        ConfidentialTransferInstruction::TransferWithFee => {
            check_num_token_accounts(account_indexes, 4)?;
            let transfer_data: TransferWithFeeInstructionData =
                *decode_instruction_data(instruction_data).map_err(|_| {
                    ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken)
                })?;
            let equality_proof_instruction_offset: i8 =
                transfer_data.equality_proof_instruction_offset;
            let transfer_amount_ciphertext_validity_proof_instruction_offset: i8 =
                transfer_data.transfer_amount_ciphertext_validity_proof_instruction_offset;
            let fee_sigma_proof_instruction_offset: i8 =
                transfer_data.fee_sigma_proof_instruction_offset;
            let fee_ciphertext_validity_proof_instruction_offset: i8 =
                transfer_data.fee_ciphertext_validity_proof_instruction_offset;
            let range_proof_instruction_offset: i8 = transfer_data.range_proof_instruction_offset;
            let mut value = json!({
                "source": account_keys[account_indexes[0] as usize].to_string(),
                "mint": account_keys[account_indexes[1] as usize].to_string(),
                "destination": account_keys[account_indexes[2] as usize].to_string(),
                "newSourceDecryptableAvailableBalance": format!("{}", transfer_data.new_source_decryptable_available_balance),
                "equalityProofInstructionOffset": equality_proof_instruction_offset,
                "transferAmountCiphertextValidityProofInstructionOffset": transfer_amount_ciphertext_validity_proof_instruction_offset,
                "feeCiphertextValidityProofInstructionOffset": fee_ciphertext_validity_proof_instruction_offset,
                "feeSigmaProofInstructionOffset": fee_sigma_proof_instruction_offset,
                "rangeProofInstructionOffset": range_proof_instruction_offset,
            });

            let mut offset = 3;
            let map = value.as_object_mut().unwrap();
            let has_sysvar = equality_proof_instruction_offset != 0
                || transfer_amount_ciphertext_validity_proof_instruction_offset != 0
                || fee_sigma_proof_instruction_offset != 0
                || fee_ciphertext_validity_proof_instruction_offset != 0
                || range_proof_instruction_offset != 0;

            if has_sysvar && offset < account_indexes.len().saturating_sub(1) {
                map.insert(
                    "instructionsSysvar".to_string(),
                    json!(account_keys[account_indexes[offset] as usize].to_string()),
                );
                offset += 1;
            }

            if equality_proof_instruction_offset == 0
                && offset < account_indexes.len().saturating_sub(1)
            {
                map.insert(
                    "equalityProofContextStateAccount".to_string(),
                    json!(account_keys[account_indexes[offset] as usize].to_string()),
                );
                offset += 1;
            }
            if transfer_amount_ciphertext_validity_proof_instruction_offset == 0
                && offset < account_indexes.len().saturating_sub(1)
            {
                map.insert(
                    "transferAmountCiphertextValidityProofContextStateAccount".to_string(),
                    json!(account_keys[account_indexes[offset] as usize].to_string()),
                );
                offset += 1;
            }
            if fee_sigma_proof_instruction_offset == 0
                && offset < account_indexes.len().saturating_sub(1)
            {
                map.insert(
                    "feeSigmaProofContextStateAccount".to_string(),
                    json!(account_keys[account_indexes[offset] as usize].to_string()),
                );
                offset += 1;
            }
            if fee_ciphertext_validity_proof_instruction_offset == 0
                && offset < account_indexes.len().saturating_sub(1)
            {
                map.insert(
                    "feeCiphertextValidityProofContextStateAccount".to_string(),
                    json!(account_keys[account_indexes[offset] as usize].to_string()),
                );
                offset += 1;
            }
            if range_proof_instruction_offset == 0
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
                instruction_type: "confidentialTransferWithFee".to_string(),
                info: value,
            })
        }
        ConfidentialTransferInstruction::ApplyPendingBalance => {
            check_num_token_accounts(account_indexes, 2)?;
            let apply_pending_balance_data: ApplyPendingBalanceData =
                *decode_instruction_data(instruction_data).map_err(|_| {
                    ParseInstructionError::InstructionNotParsable(ParsableProgram::SplToken)
                })?;
            let expected_pending_balance_credit_counter: u64 = apply_pending_balance_data
                .expected_pending_balance_credit_counter
                .into();
            let mut value = json!({
                "account": account_keys[account_indexes[0] as usize].to_string(),
                "newDecryptableAvailableBalance": format!("{}", apply_pending_balance_data.new_decryptable_available_balance),
                "expectedPendingBalanceCreditCounter": expected_pending_balance_credit_counter,

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
                instruction_type: "applyPendingConfidentialTransferBalance".to_string(),
                info: value,
            })
        }
        ConfidentialTransferInstruction::EnableConfidentialCredits => {
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
                instruction_type: "enableConfidentialTransferConfidentialCredits".to_string(),
                info: value,
            })
        }
        ConfidentialTransferInstruction::DisableConfidentialCredits => {
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
                instruction_type: "disableConfidentialTransferConfidentialCredits".to_string(),
                info: value,
            })
        }
        ConfidentialTransferInstruction::EnableNonConfidentialCredits => {
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
                instruction_type: "enableConfidentialTransferNonConfidentialCredits".to_string(),
                info: value,
            })
        }
        ConfidentialTransferInstruction::DisableNonConfidentialCredits => {
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
                instruction_type: "disableConfidentialTransferNonConfidentialCredits".to_string(),
                info: value,
            })
        }
        ConfidentialTransferInstruction::ConfigureAccountWithRegistry => {
            check_num_token_accounts(account_indexes, 3)?;
            let value = json!({
                "account": account_keys[account_indexes[0] as usize].to_string(),
                "mint": account_keys[account_indexes[1] as usize].to_string(),
                "registry": account_keys[account_indexes[2] as usize].to_string(),
            });
            Ok(ParsedInstructionEnum {
                instruction_type: "configureConfidentialAccountWithRegistry".to_string(),
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
            auth_encryption::PodAeCiphertext, elgamal::PodElGamalCiphertext,
        },
        spl_token_2022_interface::{
            extension::confidential_transfer::instruction::{
                initialize_mint, inner_configure_account, inner_empty_account, update_mint,
            },
            solana_zk_elgamal_proof_interface::proof_data::{
                BatchedGroupedCiphertext3HandlesValidityProofData, BatchedRangeProofU128Data,
                CiphertextCommitmentEqualityProofData, ZeroCiphertextProofData,
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
            Some(Pubkey::new_unique()),
            true,
            None,
        )
        .unwrap();
        check_no_panic(instruction);
    }

    #[test]
    fn test_approve() {
        let instruction = approve_account(
            &spl_token_2022_interface::id(),
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &[],
        )
        .unwrap();
        check_no_panic(instruction);
    }

    #[test]
    fn test_update() {
        let instruction = update_mint(
            &spl_token_2022_interface::id(),
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &[],
            true,
            None,
        )
        .unwrap();
        check_no_panic(instruction);
    }

    #[test]
    fn test_configure() {
        let token_account = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let authority = Pubkey::new_unique();
        let proof_ctx = Pubkey::new_unique();

        let decryptable_zero_balance = PodAeCiphertext::default();
        let maximum_pending_balance_credit_counter = 10_000;

        let offset_proof = ProofLocation::InstructionOffset(
            NonZero::new(1).unwrap(),
            &PubkeyValidityProofData::zeroed(),
        );

        let context_proof = ProofLocation::ContextStateAccount(&proof_ctx);

        // We test both proof locations to ensure the parser's offset logic holds.
        // Array of cases: (Test Name, Proof Location, Expected Offset)
        let cases = vec![
            ("Context State Account", context_proof, 0),
            ("Instruction Offset", offset_proof, 1),
        ];

        for (name, proof_location, expected_offset) in cases {
            let instruction = inner_configure_account(
                &spl_token_2022_interface::id(),
                &token_account,
                &mint,
                &decryptable_zero_balance,
                maximum_pending_balance_credit_counter,
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

            assert_eq!(
                parsed.instruction_type, "configureConfidentialTransferAccount",
                "Failed on: {}",
                name
            );
            assert_eq!(
                parsed.info["account"],
                json!(token_account.to_string()),
                "Failed on: {}",
                name
            );
            assert_eq!(
                parsed.info["mint"],
                json!(mint.to_string()),
                "Failed on: {}",
                name
            );
            assert_eq!(
                parsed.info["owner"],
                json!(authority.to_string()),
                "Failed on: {}",
                name
            );
            assert_eq!(
                parsed.info["decryptableZeroBalance"],
                json!(format!("{}", decryptable_zero_balance)),
                "Failed on: {}",
                name
            );
            assert_eq!(
                parsed.info["maximumPendingBalanceCreditCounter"],
                json!(maximum_pending_balance_credit_counter),
                "Failed on: {}",
                name
            );

            assert_eq!(
                parsed.info["proofInstructionOffset"],
                json!(expected_offset),
                "Failed on: {}",
                name
            );

            if expected_offset == 0 {
                assert_eq!(
                    parsed.info["proofContextStateAccount"],
                    json!(proof_ctx.to_string()),
                    "Proof Context mismatch on: {}",
                    name
                );
                assert!(
                    parsed.info.get("instructionsSysvar").is_none(),
                    "Sysvar should not be present on: {}",
                    name
                );
            } else {
                assert_eq!(
                    parsed.info["instructionsSysvar"],
                    json!(sysvar::instructions::id().to_string()),
                    "Sysvar mismatch on: {}",
                    name
                );
                assert!(
                    parsed.info.get("proofContextStateAccount").is_none(),
                    "Proof Context should not be present on: {}",
                    name
                );
            }
        }
    }

    #[test]
    fn test_empty_account() {
        let token_account = Pubkey::new_unique();
        let authority = Pubkey::new_unique();
        let proof_ctx = Pubkey::new_unique();

        let offset_proof = ProofLocation::InstructionOffset(
            NonZero::new(1).unwrap(),
            &ZeroCiphertextProofData::zeroed(),
        );

        let context_proof = ProofLocation::ContextStateAccount(&proof_ctx);

        // We test both proof locations to ensure the parser's offset logic holds.
        // Array of cases: (Test Name, Proof Location, Expected Offset)
        let cases = vec![
            ("Context State Account", context_proof, 0),
            ("Instruction Offset", offset_proof, 1),
        ];

        for (name, proof_location, expected_offset) in cases {
            let instruction = inner_empty_account(
                &spl_token_2022_interface::id(),
                &token_account,
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
                parsed.instruction_type, "emptyConfidentialTransferAccount",
                "Failed on: {}",
                name
            );
            assert_eq!(
                parsed.info["account"],
                json!(token_account.to_string()),
                "Failed on: {}",
                name
            );
            assert_eq!(
                parsed.info["owner"],
                json!(authority.to_string()),
                "Failed on: {}",
                name
            );

            // Expected Offset
            assert_eq!(
                parsed.info["proofInstructionOffset"],
                json!(expected_offset),
                "Failed on: {}",
                name
            );

            if expected_offset == 0 {
                assert_eq!(
                    parsed.info["proofContextStateAccount"],
                    json!(proof_ctx.to_string()),
                    "Proof Context mismatch on: {}",
                    name
                );
                assert!(
                    parsed.info.get("instructionsSysvar").is_none(),
                    "Sysvar should not be present on: {}",
                    name
                );
            } else {
                assert_eq!(
                    parsed.info["instructionsSysvar"],
                    json!(sysvar::instructions::id().to_string()),
                    "Sysvar mismatch on: {}",
                    name
                );
                assert!(
                    parsed.info.get("proofContextStateAccount").is_none(),
                    "Proof Context should not be present on: {}",
                    name
                );
            }
        }
    }

    #[test]
    fn test_deposit() {
        let token_account = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let instruction = deposit(
            &spl_token_2022_interface::id(),
            &token_account,
            &mint,
            42,
            9,
            &owner,
            &[],
        )
        .unwrap();
        let message = Message::new(&[instruction], None);
        let compiled_instruction = &message.instructions[0];
        assert_eq!(
            parse_token(
                compiled_instruction,
                &AccountKeys::new(&message.account_keys, None)
            )
            .unwrap(),
            ParsedInstructionEnum {
                instruction_type: "depositConfidentialTransfer".to_string(),
                info: json!({
                    "account": token_account.to_string(),
                    "mint": mint.to_string(),
                    "amount": 42,
                    "decimals": 9,
                    "owner": owner.to_string(),
                }),
            }
        );
    }

    #[test]
    fn test_withdraw() {
        let token_account = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let authority = Pubkey::new_unique();

        let equality_ctx = Pubkey::new_unique();
        let range_ctx = Pubkey::new_unique();

        let offset_eq = ProofLocation::InstructionOffset(
            NonZero::new(1).unwrap(),
            &CiphertextCommitmentEqualityProofData::zeroed(),
        );
        let offset_rng_2 = ProofLocation::InstructionOffset(
            NonZero::new(2).unwrap(),
            &BatchedRangeProofU64Data::zeroed(),
        );
        let offset_rng_1 = ProofLocation::InstructionOffset(
            NonZero::new(1).unwrap(),
            &BatchedRangeProofU64Data::zeroed(),
        );

        let context_eq = ProofLocation::ContextStateAccount(&equality_ctx);
        let context_rng = ProofLocation::ContextStateAccount(&range_ctx);

        // We test exhaustive combinations to ensure the parser's offset logic holds.
        // Legend:
        // 'C' = Context State Account (Proof is in a separate account)
        // 'I' = Instruction Offset (Proof is bundled in the instruction data payload)
        // Array of cases: (Test Name, Eq Proof, Rng Proof, Eq Offset, Rng Offset)
        let cases = vec![
            ("All Contexts", context_eq, context_rng, 0, 0),
            ("All Offsets", offset_eq, offset_rng_2, 1, 2),
            ("Mixed C-I", context_eq, offset_rng_1, 0, 1),
            ("Mixed I-C", offset_eq, context_rng, 1, 0),
        ];

        for (name, eq_proof, rng_proof, eq_offset, rng_offset) in cases {
            let instruction = inner_withdraw(
                &spl_token_2022_interface::id(),
                &token_account,
                &mint,
                42, // amount
                9,  // decimals
                &PodAeCiphertext::default(),
                &authority,
                &[],
                eq_proof,
                rng_proof,
            )
            .unwrap();

            check_no_panic(instruction.clone());

            let message = Message::new(&[instruction], None);
            let parsed = parse_token(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None),
            )
            .unwrap();

            assert_eq!(
                parsed.instruction_type, "withdrawConfidentialTransfer",
                "Failed on: {}",
                name
            );
            assert_eq!(
                parsed.info["account"],
                json!(token_account.to_string()),
                "Failed on: {}",
                name
            );
            assert_eq!(
                parsed.info["mint"],
                json!(mint.to_string()),
                "Failed on: {}",
                name
            );
            assert_eq!(
                parsed.info["owner"],
                json!(authority.to_string()),
                "Failed on: {}",
                name
            );
            assert_eq!(parsed.info["amount"], json!(42), "Failed on: {}", name);
            assert_eq!(parsed.info["decimals"], json!(9), "Failed on: {}", name);
            assert_eq!(
                parsed.info["newDecryptableAvailableBalance"],
                json!(format!("{}", PodAeCiphertext::default())),
                "Failed on: {}",
                name
            );

            assert_eq!(
                parsed.info["equalityProofInstructionOffset"],
                json!(eq_offset),
                "Failed on: {}",
                name
            );
            assert_eq!(
                parsed.info["rangeProofInstructionOffset"],
                json!(rng_offset),
                "Failed on: {}",
                name
            );

            // Sysvar Assertion: Only present if at least one proof relies on
            // an instruction offset
            if eq_offset != 0 || rng_offset != 0 {
                assert_eq!(
                    parsed.info["instructionsSysvar"],
                    json!(sysvar::instructions::id().to_string()),
                    "Sysvar mismatch on: {}",
                    name
                );
            } else {
                assert!(
                    parsed.info.get("instructionsSysvar").is_none(),
                    "Sysvar mismatch on: {}",
                    name
                );
            }

            if eq_offset == 0 {
                assert_eq!(
                    parsed.info["equalityProofContextStateAccount"],
                    json!(equality_ctx.to_string()),
                    "Eq Context mismatch on: {}",
                    name
                );
            } else {
                assert!(
                    parsed
                        .info
                        .get("equalityProofContextStateAccount")
                        .is_none(),
                    "Eq Context mismatch on: {}",
                    name
                );
            }

            if rng_offset == 0 {
                assert_eq!(
                    parsed.info["rangeProofContextStateAccount"],
                    json!(range_ctx.to_string()),
                    "Rng Context mismatch on: {}",
                    name
                );
            } else {
                assert!(
                    parsed.info.get("rangeProofContextStateAccount").is_none(),
                    "Rng Context mismatch on: {}",
                    name
                );
            }
        }
    }

    #[test]
    fn test_transfer() {
        let source = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let destination = Pubkey::new_unique();
        let authority = Pubkey::new_unique();

        let equality_ctx = Pubkey::new_unique();
        let validity_ctx = Pubkey::new_unique();
        let range_ctx = Pubkey::new_unique();

        let offset_eq = ProofLocation::InstructionOffset(
            NonZero::new(1).unwrap(),
            &CiphertextCommitmentEqualityProofData::zeroed(),
        );
        let offset_val = ProofLocation::InstructionOffset(
            NonZero::new(2).unwrap(),
            &BatchedGroupedCiphertext3HandlesValidityProofData::zeroed(),
        );
        let offset_rng = ProofLocation::InstructionOffset(
            NonZero::new(3).unwrap(),
            &BatchedRangeProofU128Data::zeroed(),
        );

        let context_eq = ProofLocation::ContextStateAccount(&equality_ctx);
        let context_val = ProofLocation::ContextStateAccount(&validity_ctx);
        let context_rng = ProofLocation::ContextStateAccount(&range_ctx);

        // The Confidential Transfer instruction allows proofs to be provided either via
        // pre-verified Context State accounts or directly in the Instruction data.
        // We test exhaustive combinations to ensure the parser's offset logic holds.
        // Legend:
        // 'C' = Context State Account (Proof is in a separate account)
        // 'I' = Instruction Offset (Proof is bundled in the instruction data payload)
        // Array of cases: (Test Name, Eq Proof, Val Proof, Rng Proof)
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
            ("All Offsets", offset_eq, offset_val, offset_rng, 1, 2, 3),
            ("Mixed C-I-C", context_eq, offset_val, context_rng, 0, 2, 0),
            ("Mixed I-C-I", offset_eq, context_val, offset_rng, 1, 0, 3),
        ];

        for (name, eq, val, rng, eq_offset, val_offset, rng_offset) in cases {
            let instruction = inner_transfer(
                &spl_token_2022_interface::id(),
                &source,
                &mint,
                &destination,
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

            check_no_panic(instruction.clone());

            let message = Message::new(&[instruction], None);
            let parsed = parse_token(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None),
            )
            .unwrap();

            assert_eq!(
                parsed.instruction_type, "confidentialTransfer",
                "Failed on: {}",
                name
            );
            assert_eq!(
                parsed.info["source"],
                json!(source.to_string()),
                "Failed on: {}",
                name
            );
            assert_eq!(
                parsed.info["mint"],
                json!(mint.to_string()),
                "Failed on: {}",
                name
            );
            assert_eq!(
                parsed.info["destination"],
                json!(destination.to_string()),
                "Failed on: {}",
                name
            );
            assert_eq!(
                parsed.info["owner"],
                json!(authority.to_string()),
                "Failed on: {}",
                name
            );

            assert_eq!(
                parsed.info["equalityProofInstructionOffset"],
                json!(eq_offset),
                "Failed on: {}",
                name
            );
            assert_eq!(
                parsed.info["ciphertextValidityProofInstructionOffset"],
                json!(val_offset),
                "Failed on: {}",
                name
            );
            assert_eq!(
                parsed.info["rangeProofInstructionOffset"],
                json!(rng_offset),
                "Failed on: {}",
                name
            );

            // The Instructions Sysvar is only pushed into the account list if
            // the program needs to read proof data directly from the instruction
            // payload.
            if eq_offset != 0 || val_offset != 0 || rng_offset != 0 {
                assert_eq!(
                    parsed.info["instructionsSysvar"],
                    json!(sysvar::instructions::id().to_string()),
                    "Sysvar mismatch on: {}",
                    name
                );
            } else {
                assert!(
                    parsed.info.get("instructionsSysvar").is_none(),
                    "Sysvar mismatch on: {}",
                    name
                );
            }

            if eq_offset == 0 {
                assert_eq!(
                    parsed.info["equalityProofContextStateAccount"],
                    json!(equality_ctx.to_string()),
                    "Eq Context mismatch on: {}",
                    name
                );
            } else {
                assert!(
                    parsed
                        .info
                        .get("equalityProofContextStateAccount")
                        .is_none(),
                    "Eq Context mismatch on: {}",
                    name
                );
            }

            if val_offset == 0 {
                assert_eq!(
                    parsed.info["ciphertextValidityProofContextStateAccount"],
                    json!(validity_ctx.to_string()),
                    "Val Context mismatch on: {}",
                    name
                );
            } else {
                assert!(
                    parsed
                        .info
                        .get("ciphertextValidityProofContextStateAccount")
                        .is_none(),
                    "Val Context mismatch on: {}",
                    name
                );
            }

            if rng_offset == 0 {
                assert_eq!(
                    parsed.info["rangeProofContextStateAccount"],
                    json!(range_ctx.to_string()),
                    "Rng Context mismatch on: {}",
                    name
                );
            } else {
                assert!(
                    parsed.info.get("rangeProofContextStateAccount").is_none(),
                    "Rng Context mismatch on: {}",
                    name
                );
            }
        }
    }

    #[test]
    fn test_transfer_with_fee() {
        let source = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let destination = Pubkey::new_unique();
        let authority = Pubkey::new_unique();

        let equality_ctx = Pubkey::new_unique();
        let transfer_val_ctx = Pubkey::new_unique();
        let fee_sigma_ctx = Pubkey::new_unique();
        let fee_val_ctx = Pubkey::new_unique();
        let range_ctx = Pubkey::new_unique();

        let offset_eq = ProofLocation::InstructionOffset(
            NonZero::new(1).unwrap(),
            &CiphertextCommitmentEqualityProofData::zeroed(),
        );
        let offset_transfer_val = ProofLocation::InstructionOffset(
            NonZero::new(2).unwrap(),
            &BatchedGroupedCiphertext3HandlesValidityProofData::zeroed(),
        );
        let offset_fee_sigma = ProofLocation::InstructionOffset(
            NonZero::new(3).unwrap(),
            &PercentageWithCapProofData::zeroed(),
        );
        let offset_fee_val = ProofLocation::InstructionOffset(
            NonZero::new(4).unwrap(),
            &BatchedGroupedCiphertext2HandlesValidityProofData::zeroed(),
        );
        let offset_rng = ProofLocation::InstructionOffset(
            NonZero::new(5).unwrap(),
            &BatchedRangeProofU256Data::zeroed(),
        );

        let context_eq = ProofLocation::ContextStateAccount(&equality_ctx);
        let context_transfer_val = ProofLocation::ContextStateAccount(&transfer_val_ctx);
        let context_fee_sigma = ProofLocation::ContextStateAccount(&fee_sigma_ctx);
        let context_fee_val = ProofLocation::ContextStateAccount(&fee_val_ctx);
        let context_rng = ProofLocation::ContextStateAccount(&range_ctx);

        // We test exhaustive combinations to ensure the parser's offset logic holds.
        // Legend:
        // 'C' = Context State Account (Proof is in a separate account)
        // 'I' = Instruction Offset (Proof is bundled in the instruction data payload)
        // Array of cases: (Test Name, Eq, TransferVal, FeeSigma, FeeVal, Rng,
        // EqOffset, TransferValOffset, FeeSigmaOffset, FeeValOffset, RngOffset)
        let cases = vec![
            (
                "All Contexts",
                context_eq,
                context_transfer_val,
                context_fee_sigma,
                context_fee_val,
                context_rng,
                0,
                0,
                0,
                0,
                0,
            ),
            (
                "All Offsets",
                offset_eq,
                offset_transfer_val,
                offset_fee_sigma,
                offset_fee_val,
                offset_rng,
                1,
                2,
                3,
                4,
                5,
            ),
            (
                "Mixed C-I-C-I-C",
                context_eq,
                offset_transfer_val,
                context_fee_sigma,
                offset_fee_val,
                context_rng,
                0,
                2,
                0,
                4,
                0,
            ),
            (
                "Mixed I-C-I-C-I",
                offset_eq,
                context_transfer_val,
                offset_fee_sigma,
                context_fee_val,
                offset_rng,
                1,
                0,
                3,
                0,
                5,
            ),
        ];

        for (
            name,
            eq,
            transfer_val,
            fee_sigma,
            fee_val,
            rng,
            eq_offset,
            transfer_val_offset,
            fee_sigma_offset,
            fee_val_offset,
            rng_offset,
        ) in cases
        {
            let instruction = inner_transfer_with_fee(
                &spl_token_2022_interface::id(),
                &source,
                &mint,
                &destination,
                &PodAeCiphertext::default(),
                &PodElGamalCiphertext::default(),
                &PodElGamalCiphertext::default(),
                &authority,
                &[],
                eq,
                transfer_val,
                fee_sigma,
                fee_val,
                rng,
            )
            .unwrap();

            check_no_panic(instruction.clone());

            let message = Message::new(&[instruction], None);
            let parsed = parse_token(
                &message.instructions[0],
                &AccountKeys::new(&message.account_keys, None),
            )
            .unwrap();

            assert_eq!(
                parsed.instruction_type, "confidentialTransferWithFee",
                "Failed on: {}",
                name
            );
            assert_eq!(
                parsed.info["source"],
                json!(source.to_string()),
                "Failed on: {}",
                name
            );
            assert_eq!(
                parsed.info["mint"],
                json!(mint.to_string()),
                "Failed on: {}",
                name
            );
            assert_eq!(
                parsed.info["destination"],
                json!(destination.to_string()),
                "Failed on: {}",
                name
            );
            assert_eq!(
                parsed.info["owner"],
                json!(authority.to_string()),
                "Failed on: {}",
                name
            );

            assert_eq!(
                parsed.info["equalityProofInstructionOffset"],
                json!(eq_offset),
                "Failed on: {}",
                name
            );
            assert_eq!(
                parsed.info["transferAmountCiphertextValidityProofInstructionOffset"],
                json!(transfer_val_offset),
                "Failed on: {}",
                name
            );
            assert_eq!(
                parsed.info["feeSigmaProofInstructionOffset"],
                json!(fee_sigma_offset),
                "Failed on: {}",
                name
            );
            assert_eq!(
                parsed.info["feeCiphertextValidityProofInstructionOffset"],
                json!(fee_val_offset),
                "Failed on: {}",
                name
            );
            assert_eq!(
                parsed.info["rangeProofInstructionOffset"],
                json!(rng_offset),
                "Failed on: {}",
                name
            );

            // Sysvar Assertion: Only present if at least one proof relies on an
            // instruction offset
            if eq_offset != 0
                || transfer_val_offset != 0
                || fee_sigma_offset != 0
                || fee_val_offset != 0
                || rng_offset != 0
            {
                assert_eq!(
                    parsed.info["instructionsSysvar"],
                    json!(sysvar::instructions::id().to_string()),
                    "Sysvar mismatch on: {}",
                    name
                );
            } else {
                assert!(
                    parsed.info.get("instructionsSysvar").is_none(),
                    "Sysvar mismatch on: {}",
                    name
                );
            }

            if eq_offset == 0 {
                assert_eq!(
                    parsed.info["equalityProofContextStateAccount"],
                    json!(equality_ctx.to_string()),
                    "Eq Context mismatch on: {}",
                    name
                );
            } else {
                assert!(
                    parsed
                        .info
                        .get("equalityProofContextStateAccount")
                        .is_none(),
                    "Eq Context mismatch on: {}",
                    name
                );
            }

            if transfer_val_offset == 0 {
                assert_eq!(
                    parsed.info["transferAmountCiphertextValidityProofContextStateAccount"],
                    json!(transfer_val_ctx.to_string()),
                    "Transfer Val Context mismatch on: {}",
                    name
                );
            } else {
                assert!(
                    parsed
                        .info
                        .get("transferAmountCiphertextValidityProofContextStateAccount")
                        .is_none(),
                    "Transfer Val Context mismatch on: {}",
                    name
                );
            }

            if fee_sigma_offset == 0 {
                assert_eq!(
                    parsed.info["feeSigmaProofContextStateAccount"],
                    json!(fee_sigma_ctx.to_string()),
                    "Fee Sigma Context mismatch on: {}",
                    name
                );
            } else {
                assert!(
                    parsed
                        .info
                        .get("feeSigmaProofContextStateAccount")
                        .is_none(),
                    "Fee Sigma Context mismatch on: {}",
                    name
                );
            }

            if fee_val_offset == 0 {
                assert_eq!(
                    parsed.info["feeCiphertextValidityProofContextStateAccount"],
                    json!(fee_val_ctx.to_string()),
                    "Fee Val Context mismatch on: {}",
                    name
                );
            } else {
                assert!(
                    parsed
                        .info
                        .get("feeCiphertextValidityProofContextStateAccount")
                        .is_none(),
                    "Fee Val Context mismatch on: {}",
                    name
                );
            }

            if rng_offset == 0 {
                assert_eq!(
                    parsed.info["rangeProofContextStateAccount"],
                    json!(range_ctx.to_string()),
                    "Rng Context mismatch on: {}",
                    name
                );
            } else {
                assert!(
                    parsed.info.get("rangeProofContextStateAccount").is_none(),
                    "Rng Context mismatch on: {}",
                    name
                );
            }
        }
    }
}
