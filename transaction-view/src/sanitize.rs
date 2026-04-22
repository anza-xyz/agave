use {
    crate::{
        result::{Result, TransactionViewError},
        signature_frame::MAX_SIGNATURES_PER_PACKET,
        transaction_data::TransactionData,
        transaction_version::TransactionVersion,
        transaction_view::UnsanitizedTransactionView,
    },
    solana_program_runtime::execution_budget::{MAX_HEAP_FRAME_BYTES, MIN_HEAP_FRAME_BYTES},
};

pub(crate) fn sanitize(
    view: &UnsanitizedTransactionView<impl TransactionData>,
    enable_instruction_accounts_limit: bool,
) -> Result<()> {
    sanitize_transaction_size(view)?;
    sanitize_message_header(view)?;
    sanitize_config(view)?;
    sanitize_signatures(view)?;
    sanitize_account_access(view)?;
    sanitize_instructions(view, enable_instruction_accounts_limit)?;
    sanitize_address_table_lookups(view)
}

/// Transaction constraints:
/// * size <= 4096 bytes
fn sanitize_transaction_size(
    view: &UnsanitizedTransactionView<impl TransactionData>,
) -> Result<()> {
    let max_transaction_size = match view.version() {
        TransactionVersion::Legacy | TransactionVersion::V0 => solana_packet::PACKET_DATA_SIZE,
        TransactionVersion::V1 => solana_message::v1::MAX_TRANSACTION_SIZE,
    };

    if view.data().len() > max_transaction_size {
        return Err(TransactionViewError::SanitizeError);
    }
    Ok(())
}

/// message header constraints:
/// * num_required_signatures >= 1
/// * num_readonly_signed_accounts < num_required_signatures (fee payer must be writable)
/// * num_readonly_unsigned_accounts <= (num_addresses - num_required_signatures)
fn sanitize_message_header(view: &UnsanitizedTransactionView<impl TransactionData>) -> Result<()> {
    if view.num_required_signatures() < 1 {
        return Err(TransactionViewError::SanitizeError);
    }

    if view.num_readonly_signed_static_accounts() >= view.num_required_signatures() {
        return Err(TransactionViewError::SanitizeError);
    }

    // Check there is no overlap of signing area and readonly non-signing area.
    // We have already checked that `num_required_signatures` is less than or equal to `num_static_account_keys`,
    // so it is safe to use wrapping arithmetic.
    if view.num_readonly_unsigned_static_accounts()
        > view
            .num_static_account_keys()
            .wrapping_sub(view.num_required_signatures())
    {
        return Err(TransactionViewError::SanitizeError);
    }

    Ok(())
}

/// Config Constraints:
/// * heap_size must be multiples of 1024, if specified
fn sanitize_config(view: &UnsanitizedTransactionView<impl TransactionData>) -> Result<()> {
    #[allow(clippy::collapsible_if)]
    if let Some(requested_heap_bytes) = view
        .transaction_config()
        .and_then(|config| config.requested_heap_size())
    {
        if !(MIN_HEAP_FRAME_BYTES..=MAX_HEAP_FRAME_BYTES).contains(&requested_heap_bytes)
            || !requested_heap_bytes.is_multiple_of(1024)
        {
            return Err(TransactionViewError::SanitizeError);
        }
    }

    Ok(())
}

/// Sigantures Constraint:
/// * Number of signatures must equal: num_required_signatures
/// * Max signatures <= 12
fn sanitize_signatures(view: &UnsanitizedTransactionView<impl TransactionData>) -> Result<()> {
    // Check the required number of signatures matches the number of signatures.
    if view.num_signatures() != view.num_required_signatures() {
        return Err(TransactionViewError::SanitizeError);
    }

    if view.num_signatures() > MAX_SIGNATURES_PER_PACKET {
        return Err(TransactionViewError::SanitizeError);
    }

    // Each signature is associated with a unique static public key.
    // Check that there are at least as many static account keys as signatures.
    if view.num_static_account_keys() < view.num_signatures() {
        return Err(TransactionViewError::SanitizeError);
    }

    Ok(())
}

/// Accounts (aka Addresses) Constraints:
/// * for v1: 1 <= NumAddresses <= 64
///   * legacy/v0 uses current limits of: num_accounts <= 256 (u8 bound)
/// * No duplicate addresses
fn sanitize_account_access(view: &UnsanitizedTransactionView<impl TransactionData>) -> Result<()> {
    let addresses_limit = match view.version() {
        TransactionVersion::Legacy | TransactionVersion::V0 => 256,
        TransactionVersion::V1 => 64,
    };

    if total_number_of_accounts(view) > addresses_limit {
        return Err(TransactionViewError::SanitizeError);
    }

    // No duplicated accounts
    // Note: This check is performed downstream in `validate_account_locks()`.
    // It is skipped here to avoid redundant work on the hot path.

    Ok(())
}

/// Instructions Constraints
/// * NumInstructions <= 64
/// * Per instruction:
///   * 0 < program_id_index < MaxProgramIdIndex
///   * all account indices < MaxAccountIndex
fn sanitize_instructions(
    view: &UnsanitizedTransactionView<impl TransactionData>,
    enable_instruction_accounts_limit: bool,
) -> Result<()> {
    // SIMD-160: transaction can not have more than 64 top level instructions
    if usize::from(view.num_instructions())
        > solana_transaction_context::MAX_INSTRUCTION_TRACE_LENGTH
    {
        return Err(TransactionViewError::SanitizeError);
    }

    // already verified there is at least one static account.
    let max_program_id_index = view.num_static_account_keys().wrapping_sub(1);
    // verified that there are no more than 256 accounts in `sanitize_account_access`
    let max_account_index = total_number_of_accounts(view).wrapping_sub(1) as u8;

    for instruction in view.instructions_iter() {
        // Check that program indexes are static account keys.
        if instruction.program_id_index > max_program_id_index {
            return Err(TransactionViewError::SanitizeError);
        }

        // Check that the program index is not the fee-payer.
        if instruction.program_id_index == 0 {
            return Err(TransactionViewError::SanitizeError);
        }

        // Check that all account indexes are valid.
        for account_index in instruction.accounts.iter().copied() {
            if account_index > max_account_index {
                return Err(TransactionViewError::SanitizeError);
            }
        }

        if enable_instruction_accounts_limit
            && instruction.accounts.len() > solana_transaction_context::MAX_ACCOUNTS_PER_INSTRUCTION
        {
            return Err(TransactionViewError::SanitizeError);
        }
    }

    Ok(())
}

fn sanitize_address_table_lookups(
    view: &UnsanitizedTransactionView<impl TransactionData>,
) -> Result<()> {
    for address_table_lookup in view.address_table_lookup_iter() {
        // Check that there is at least one account lookup.
        if address_table_lookup.writable_indexes.is_empty()
            && address_table_lookup.readonly_indexes.is_empty()
        {
            return Err(TransactionViewError::SanitizeError);
        }
    }

    Ok(())
}

fn total_number_of_accounts(view: &UnsanitizedTransactionView<impl TransactionData>) -> u16 {
    u16::from(view.num_static_account_keys())
        .saturating_add(view.total_writable_lookup_accounts())
        .saturating_add(view.total_readonly_lookup_accounts())
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::transaction_view::TransactionView,
        solana_hash::Hash,
        solana_message::{
            Message, MessageHeader, VersionedMessage,
            compiled_instruction::CompiledInstruction,
            v0::{self, MessageAddressTableLookup},
            v1::{self, TransactionConfig},
        },
        solana_pubkey::Pubkey,
        solana_signature::Signature,
        solana_system_interface::instruction as system_instruction,
        solana_transaction::versioned::VersionedTransaction,
    };

    fn create_legacy_transaction(
        num_signatures: u8,
        header: MessageHeader,
        account_keys: Vec<Pubkey>,
        instructions: Vec<CompiledInstruction>,
    ) -> VersionedTransaction {
        VersionedTransaction {
            signatures: vec![Signature::default(); num_signatures as usize],
            message: VersionedMessage::Legacy(Message {
                header,
                account_keys,
                recent_blockhash: Hash::default(),
                instructions,
            }),
        }
    }

    fn create_v0_transaction(
        num_signatures: u8,
        header: MessageHeader,
        account_keys: Vec<Pubkey>,
        instructions: Vec<CompiledInstruction>,
        address_table_lookups: Vec<MessageAddressTableLookup>,
    ) -> VersionedTransaction {
        VersionedTransaction {
            signatures: vec![Signature::default(); num_signatures as usize],
            message: VersionedMessage::V0(v0::Message {
                header,
                account_keys,
                recent_blockhash: Hash::default(),
                instructions,
                address_table_lookups,
            }),
        }
    }

    fn create_v1_transaction(
        num_signatures: u8,
        header: MessageHeader,
        account_keys: Vec<Pubkey>,
        instructions: Vec<CompiledInstruction>,
        config: TransactionConfig,
    ) -> VersionedTransaction {
        VersionedTransaction {
            signatures: vec![Signature::default(); num_signatures as usize],
            message: VersionedMessage::V1(v1::Message {
                header,
                account_keys,
                lifetime_specifier: Hash::default(),
                instructions,
                config,
            }),
        }
    }

    fn multiple_transfers() -> VersionedTransaction {
        let payer = Pubkey::new_unique();
        VersionedTransaction {
            signatures: vec![Signature::default()], // 1 signature to be valid.
            message: VersionedMessage::Legacy(Message::new(
                &[
                    system_instruction::transfer(&payer, &Pubkey::new_unique(), 1),
                    system_instruction::transfer(&payer, &Pubkey::new_unique(), 1),
                ],
                Some(&payer),
            )),
        }
    }

    #[test]
    fn test_sanitize_multiple_transfers() {
        let transaction = multiple_transfers();
        let data = wincode::serialize(&transaction).unwrap();
        let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
        assert!(view.sanitize(true).is_ok());
    }

    #[test]
    fn test_sanitize_transaction_size_too_large() {
        let account_keys = vec![Pubkey::new_unique(), Pubkey::new_unique()];
        let transaction = create_legacy_transaction(
            1,
            MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 1,
            },
            account_keys,
            vec![CompiledInstruction {
                program_id_index: 1,
                accounts: vec![0],
                data: vec![0; 5000],
            }],
        );
        let data = wincode::serialize(&transaction).unwrap();
        let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
        assert_eq!(
            sanitize_transaction_size(&view),
            Err(TransactionViewError::SanitizeError)
        );
    }

    #[test]
    fn test_sanitize_signatures() {
        // Too few signatures.
        {
            let transaction = create_legacy_transaction(
                1,
                MessageHeader {
                    num_required_signatures: 2,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 0,
                },
                (0..3).map(|_| Pubkey::new_unique()).collect(),
                vec![],
            );
            let data = wincode::serialize(&transaction).unwrap();
            let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
            assert_eq!(
                sanitize_signatures(&view),
                Err(TransactionViewError::SanitizeError)
            );
        }

        // Too many signatures.
        {
            let transaction = create_legacy_transaction(
                2,
                MessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 0,
                },
                (0..3).map(|_| Pubkey::new_unique()).collect(),
                vec![],
            );
            let data = wincode::serialize(&transaction).unwrap();
            let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
            assert_eq!(
                sanitize_signatures(&view),
                Err(TransactionViewError::SanitizeError)
            );
        }

        // Not enough static accounts.
        {
            let transaction = create_legacy_transaction(
                2,
                MessageHeader {
                    num_required_signatures: 2,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 0,
                },
                (0..1).map(|_| Pubkey::new_unique()).collect(),
                vec![],
            );
            let data = wincode::serialize(&transaction).unwrap();
            let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
            assert_eq!(
                sanitize_signatures(&view),
                Err(TransactionViewError::SanitizeError)
            );
        }

        // More than 12 signatures.
        {
            let transaction = create_legacy_transaction(
                13,
                MessageHeader {
                    num_required_signatures: 13,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 0,
                },
                (0..13).map(|_| Pubkey::new_unique()).collect(),
                vec![],
            );
            let data = wincode::serialize(&transaction).unwrap();
            let view = TransactionView::try_new_unsanitized(data.as_ref());
            // SignatureFrame validates number of signatures, it throw ParseError if
            // it is less than 12
            assert!(matches!(view, Err(TransactionViewError::ParseError)));
        }

        {
            let transaction = create_v1_transaction(
                13,
                MessageHeader {
                    num_required_signatures: 13,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 0,
                },
                (0..13).map(|_| Pubkey::new_unique()).collect(),
                vec![],
                TransactionConfig::empty(),
            );
            let data = wincode::serialize(&transaction).unwrap();
            let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
            assert_eq!(
                sanitize_signatures(&view),
                Err(TransactionViewError::SanitizeError)
            );
        }

        // Not enough static accounts.
        {
            let transaction = create_legacy_transaction(
                2,
                MessageHeader {
                    num_required_signatures: 2,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 0,
                },
                (0..1).map(|_| Pubkey::new_unique()).collect(),
                vec![],
            );
            let data = wincode::serialize(&transaction).unwrap();
            let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
            assert_eq!(
                sanitize_signatures(&view),
                Err(TransactionViewError::SanitizeError)
            );
        }

        // Not enough static accounts - with look up accounts
        {
            let transaction = create_v0_transaction(
                2,
                MessageHeader {
                    num_required_signatures: 2,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 0,
                },
                (0..1).map(|_| Pubkey::new_unique()).collect(),
                vec![],
                vec![MessageAddressTableLookup {
                    account_key: Pubkey::new_unique(),
                    writable_indexes: vec![0, 1, 2, 3, 4, 5],
                    readonly_indexes: vec![6, 7, 8],
                }],
            );
            let data = wincode::serialize(&transaction).unwrap();
            let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
            assert_eq!(
                sanitize_signatures(&view),
                Err(TransactionViewError::SanitizeError)
            );
        }
    }

    #[test]
    fn test_sanitize_account_access() {
        // num_required_signatures must be >= 1.
        {
            let transaction = create_legacy_transaction(
                0,
                MessageHeader {
                    num_required_signatures: 0,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 0,
                },
                vec![Pubkey::new_unique()],
                vec![],
            );
            let data = wincode::serialize(&transaction).unwrap();
            let view = TransactionView::try_new_unsanitized(data.as_ref());
            // SignatureFrame validates number of signatures, it throw ParseError if
            // it is less than 1
            assert!(matches!(view, Err(TransactionViewError::ParseError)));
        }
        {
            let transaction = create_v1_transaction(
                0,
                MessageHeader {
                    num_required_signatures: 0,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 0,
                },
                vec![Pubkey::new_unique()],
                vec![],
                TransactionConfig::empty(),
            );
            let data = wincode::serialize(&transaction).unwrap();
            let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
            assert_eq!(
                sanitize_message_header(&view),
                Err(TransactionViewError::SanitizeError)
            );
        }

        // Overlap of signing and readonly non-signing accounts.
        {
            let transaction = create_legacy_transaction(
                1,
                MessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 2,
                },
                (0..2).map(|_| Pubkey::new_unique()).collect(),
                vec![],
            );
            let data = wincode::serialize(&transaction).unwrap();
            let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
            assert_eq!(
                sanitize_message_header(&view),
                Err(TransactionViewError::SanitizeError)
            );
        }

        // Not enough writable accounts.
        {
            let transaction = create_legacy_transaction(
                1,
                MessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 1,
                    num_readonly_unsigned_accounts: 0,
                },
                (0..2).map(|_| Pubkey::new_unique()).collect(),
                vec![],
            );
            let data = wincode::serialize(&transaction).unwrap();
            let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
            assert_eq!(
                sanitize_message_header(&view),
                Err(TransactionViewError::SanitizeError)
            );
        }

        // Too many accounts in legacy/v0
        {
            let transaction = create_v0_transaction(
                2,
                MessageHeader {
                    num_required_signatures: 2,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 0,
                },
                (0..1).map(|_| Pubkey::new_unique()).collect(),
                vec![],
                vec![
                    MessageAddressTableLookup {
                        account_key: Pubkey::new_unique(),
                        writable_indexes: (0..100).collect(),
                        readonly_indexes: (100..200).collect(),
                    },
                    MessageAddressTableLookup {
                        account_key: Pubkey::new_unique(),
                        writable_indexes: (100..200).collect(),
                        readonly_indexes: (0..100).collect(),
                    },
                ],
            );
            let data = wincode::serialize(&transaction).unwrap();
            let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
            assert_eq!(
                sanitize_account_access(&view),
                Err(TransactionViewError::SanitizeError)
            );
        }

        // V1: too many static accounts.
        {
            let transaction = create_v1_transaction(
                1,
                MessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 63,
                },
                (0..65).map(|_| Pubkey::new_unique()).collect(),
                vec![],
                TransactionConfig::empty(),
            );
            let data = wincode::serialize(&transaction).unwrap();
            let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
            assert_eq!(
                sanitize_account_access(&view),
                Err(TransactionViewError::SanitizeError)
            );
        }
    }

    #[test]
    fn test_sanitize_instructions() {
        let num_signatures = 1;
        let header = MessageHeader {
            num_required_signatures: 1,
            num_readonly_signed_accounts: 0,
            num_readonly_unsigned_accounts: 1,
        };
        let account_keys = vec![
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        ];
        let valid_instructions = vec![
            CompiledInstruction {
                program_id_index: 1,
                accounts: vec![0, 1],
                data: vec![1, 2, 3],
            },
            CompiledInstruction {
                program_id_index: 2,
                accounts: vec![1, 0],
                data: vec![3, 2, 1, 4],
            },
        ];
        let atls = vec![MessageAddressTableLookup {
            account_key: Pubkey::new_unique(),
            writable_indexes: vec![0, 1],
            readonly_indexes: vec![2],
        }];

        // Verify that the unmodified transaction(s) are valid/sanitized.
        {
            let transaction = create_legacy_transaction(
                num_signatures,
                header,
                account_keys.clone(),
                valid_instructions.clone(),
            );
            let data = wincode::serialize(&transaction).unwrap();
            let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
            assert!(sanitize_instructions(&view, true).is_ok());

            let transaction = create_v0_transaction(
                num_signatures,
                header,
                account_keys.clone(),
                valid_instructions.clone(),
                atls.clone(),
            );
            let data = wincode::serialize(&transaction).unwrap();
            let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
            assert!(sanitize_instructions(&view, true).is_ok());
        }

        for instruction_index in 0..valid_instructions.len() {
            // Invalid program index.
            {
                let mut instructions = valid_instructions.clone();
                instructions[instruction_index].program_id_index = account_keys.len() as u8;
                let transaction = create_legacy_transaction(
                    num_signatures,
                    header,
                    account_keys.clone(),
                    instructions,
                );
                let data = wincode::serialize(&transaction).unwrap();
                let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
                assert_eq!(
                    sanitize_instructions(&view, true),
                    Err(TransactionViewError::SanitizeError)
                );
            }

            // Invalid program index with lookups.
            {
                let mut instructions = valid_instructions.clone();
                instructions[instruction_index].program_id_index = account_keys.len() as u8;
                let transaction = create_v0_transaction(
                    num_signatures,
                    header,
                    account_keys.clone(),
                    instructions,
                    atls.clone(),
                );
                let data = wincode::serialize(&transaction).unwrap();
                let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
                assert_eq!(
                    sanitize_instructions(&view, true),
                    Err(TransactionViewError::SanitizeError)
                );
            }

            // Program index is fee-payer.
            {
                let mut instructions = valid_instructions.clone();
                instructions[instruction_index].program_id_index = 0;
                let transaction = create_legacy_transaction(
                    num_signatures,
                    header,
                    account_keys.clone(),
                    instructions,
                );
                let data = wincode::serialize(&transaction).unwrap();
                let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
                assert_eq!(
                    sanitize_instructions(&view, true),
                    Err(TransactionViewError::SanitizeError)
                );
            }

            // Invalid account index.
            {
                let mut instructions = valid_instructions.clone();
                instructions[instruction_index]
                    .accounts
                    .push(account_keys.len() as u8);
                let transaction = create_legacy_transaction(
                    num_signatures,
                    header,
                    account_keys.clone(),
                    instructions,
                );
                let data = wincode::serialize(&transaction).unwrap();
                let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
                assert_eq!(
                    sanitize_instructions(&view, true),
                    Err(TransactionViewError::SanitizeError)
                );
            }

            // Invalid account index with v0.
            {
                let num_lookup_accounts =
                    atls[0].writable_indexes.len() + atls[0].readonly_indexes.len();
                let total_accounts = (account_keys.len() + num_lookup_accounts) as u8;
                let mut instructions = valid_instructions.clone();
                instructions[instruction_index]
                    .accounts
                    .push(total_accounts);
                let transaction = create_v0_transaction(
                    num_signatures,
                    header,
                    account_keys.clone(),
                    instructions,
                    atls.clone(),
                );
                let data = wincode::serialize(&transaction).unwrap();
                let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
                assert_eq!(
                    sanitize_instructions(&view, true),
                    Err(TransactionViewError::SanitizeError)
                );
            }
        }

        // SIMD-0160, too many instructions are invalid
        {
            let too_many_instructions: Vec<_> = valid_instructions
                .iter()
                .cycle()
                .take(65)
                .cloned()
                .collect();
            let transaction = create_legacy_transaction(
                num_signatures,
                header,
                account_keys.clone(),
                too_many_instructions.clone(),
            );
            let data = wincode::serialize(&transaction).unwrap();
            let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
            assert_eq!(
                sanitize_instructions(&view, true),
                Err(TransactionViewError::SanitizeError)
            );

            let transaction = create_v0_transaction(
                num_signatures,
                header,
                account_keys.clone(),
                too_many_instructions.clone(),
                atls.clone(),
            );
            let data = wincode::serialize(&transaction).unwrap();
            let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
            assert_eq!(
                sanitize_instructions(&view, true),
                Err(TransactionViewError::SanitizeError)
            );
        }

        // SIMD-406: Limit instruction accounts to 255
        {
            let mut accounts: Vec<u8> = vec![0; 254];
            accounts.push(1);
            accounts.push(2);
            let instr = CompiledInstruction::new_from_raw_parts(2, Vec::new(), accounts);
            let transaction = create_legacy_transaction(
                num_signatures,
                header,
                account_keys.clone(),
                vec![instr],
            );
            let data = wincode::serialize(&transaction).unwrap();
            let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
            assert_eq!(
                sanitize_instructions(&view, true),
                Err(TransactionViewError::SanitizeError)
            );
        }

        // SIMD-406: Limit instruction accounts to 255
        {
            let mut accounts: Vec<u8> = vec![0; 254];
            accounts.push(1);
            let instr = CompiledInstruction::new_from_raw_parts(2, Vec::new(), accounts);
            let transaction = create_legacy_transaction(
                num_signatures,
                header,
                account_keys.clone(),
                vec![instr],
            );
            let data = wincode::serialize(&transaction).unwrap();
            let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
            // Exactly 255 accounts must pass sanitization.
            assert!(sanitize_instructions(&view, true).is_ok());
        }
    }

    #[test]
    fn test_sanitize_address_table_lookups() {
        fn create_transaction(empty_index: usize) -> VersionedTransaction {
            let payer = Pubkey::new_unique();
            let mut address_table_lookups = vec![
                MessageAddressTableLookup {
                    account_key: Pubkey::new_unique(),
                    writable_indexes: vec![0, 1],
                    readonly_indexes: vec![],
                },
                MessageAddressTableLookup {
                    account_key: Pubkey::new_unique(),
                    writable_indexes: vec![0, 1],
                    readonly_indexes: vec![],
                },
            ];
            address_table_lookups[empty_index].writable_indexes.clear();
            create_v0_transaction(
                1,
                MessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 0,
                },
                vec![payer],
                vec![],
                address_table_lookups,
            )
        }

        for empty_index in 0..2 {
            let transaction = create_transaction(empty_index);
            assert_eq!(
                transaction.message.address_table_lookups().unwrap().len(),
                2
            );
            let data = wincode::serialize(&transaction).unwrap();
            let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
            assert_eq!(
                sanitize_address_table_lookups(&view),
                Err(TransactionViewError::SanitizeError)
            );
        }
    }

    #[test]
    fn test_sanitize_config() {
        // Valid min heap size.
        {
            let transaction = create_v1_transaction(
                1,
                MessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 1,
                },
                (0..2).map(|_| Pubkey::new_unique()).collect(),
                vec![],
                TransactionConfig::empty().with_heap_size(MIN_HEAP_FRAME_BYTES),
            );
            let data = wincode::serialize(&transaction).unwrap();
            let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
            assert!(sanitize_config(&view).is_ok());
        }

        // Valid max heap size.
        {
            let transaction = create_v1_transaction(
                1,
                MessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 1,
                },
                (0..2).map(|_| Pubkey::new_unique()).collect(),
                vec![],
                TransactionConfig::empty().with_heap_size(MAX_HEAP_FRAME_BYTES),
            );
            let data = wincode::serialize(&transaction).unwrap();
            let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
            assert!(sanitize_config(&view).is_ok());
        }

        // Heap size below min.
        {
            let transaction = create_v1_transaction(
                1,
                MessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 1,
                },
                (0..2).map(|_| Pubkey::new_unique()).collect(),
                vec![],
                TransactionConfig::empty().with_heap_size(MIN_HEAP_FRAME_BYTES - 1),
            );
            let data = wincode::serialize(&transaction).unwrap();
            let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
            assert_eq!(
                sanitize_config(&view),
                Err(TransactionViewError::SanitizeError)
            );
        }

        // Heap size above max.
        {
            let transaction = create_v1_transaction(
                1,
                MessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 1,
                },
                (0..2).map(|_| Pubkey::new_unique()).collect(),
                vec![],
                TransactionConfig::empty().with_heap_size(MAX_HEAP_FRAME_BYTES + 1),
            );
            let data = wincode::serialize(&transaction).unwrap();
            let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
            assert_eq!(
                sanitize_config(&view),
                Err(TransactionViewError::SanitizeError)
            );
        }

        // Heap size not multiple of 1024.
        {
            let transaction = create_v1_transaction(
                1,
                MessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 1,
                },
                (0..2).map(|_| Pubkey::new_unique()).collect(),
                vec![],
                TransactionConfig::empty().with_heap_size(MIN_HEAP_FRAME_BYTES + 1),
            );
            let data = wincode::serialize(&transaction).unwrap();
            let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
            assert_eq!(
                sanitize_config(&view),
                Err(TransactionViewError::SanitizeError)
            );
        }

        // Config is not set, default is OK
        {
            let transaction = create_v1_transaction(
                1,
                MessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 1,
                },
                (0..2).map(|_| Pubkey::new_unique()).collect(),
                vec![],
                TransactionConfig::empty(),
            );
            let data = wincode::serialize(&transaction).unwrap();
            let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
            assert!(sanitize_config(&view).is_ok());
        }
    }
}
