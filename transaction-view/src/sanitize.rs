use crate::{
    result::{Result, TransactionViewError},
    transaction_data::TransactionData,
    transaction_view::TransactionView,
};

// alias for convenience
type UnsanitizedTransactionView<D> = TransactionView<false, D>;
type SanitizedTransactionView<D> = TransactionView<true, D>;

pub fn sanitize<D: TransactionData>(
    view: UnsanitizedTransactionView<D>,
) -> Result<SanitizedTransactionView<D>> {
    sanitize_signatures(&view)?;
    sanitize_account_access(&view)?;
    sanitize_instructions(&view)?;
    sanitize_address_table_lookups(&view)?;

    Ok(SanitizedTransactionView {
        data: view.data,
        meta: view.meta,
    })
}

fn sanitize_signatures(view: &UnsanitizedTransactionView<impl TransactionData>) -> Result<()> {
    // Check the required number of signatures matches the number of signatures.
    if view.num_signatures() != view.num_required_signatures() {
        return Err(TransactionViewError::SanitizeError);
    }

    // Each signature is associated with a unique static public key.
    // Check that there are at least as many static account keys as signatures.
    if view.num_static_account_keys() < view.num_signatures() {
        return Err(TransactionViewError::SanitizeError);
    }

    Ok(())
}

fn sanitize_account_access(view: &UnsanitizedTransactionView<impl TransactionData>) -> Result<()> {
    // Check there is no overlap of signing area and readonly non-signing area.
    // We have already checked that `num_required_signatures` is less than or equal to `num_static_account_keys`,
    // so it is safe to use wrapping arithmetic.
    if view.num_readonly_unsigned_accounts()
        > view
            .num_static_account_keys()
            .wrapping_sub(view.num_required_signatures())
    {
        return Err(TransactionViewError::SanitizeError);
    }

    // Check there is at least 1 writable fee-payer account.
    if view.num_readonly_signed_accounts() >= view.num_required_signatures() {
        return Err(TransactionViewError::SanitizeError);
    }

    // Check there are not more than 256 accounts.
    if total_number_of_accounts(view) > 256 {
        return Err(TransactionViewError::SanitizeError);
    }

    Ok(())
}

fn sanitize_instructions(view: &UnsanitizedTransactionView<impl TransactionData>) -> Result<()> {
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
        crate::{
            address_table_lookup_meta::AddressTableLookupMeta,
            instructions_meta::InstructionsMeta,
            message_header_meta::{MessageHeaderMeta, TransactionVersion},
            signature_meta::SignatureMeta,
            static_account_keys_meta::StaticAccountKeysMeta,
            transaction_meta::TransactionMeta,
        },
        solana_sdk::{
            hash::Hash,
            message::{
                v0::{self, MessageAddressTableLookup},
                Message, MessageHeader, VersionedMessage,
            },
            pubkey::Pubkey,
            signature::Signature,
            system_instruction,
            transaction::VersionedTransaction,
        },
    };

    // Construct a dummy metadata object for testing.
    // Most sanitization checks are based on the counts stored in this metadata
    // rather than checking actual data. Using a mutable dummy object allows for
    // far simpler testing.
    fn dummy_metadata() -> TransactionMeta {
        TransactionMeta {
            signature: SignatureMeta {
                num_signatures: 1,
                offset: 1,
            },
            message_header: MessageHeaderMeta {
                offset: 0,
                version: TransactionVersion::Legacy,
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 0,
            },
            static_account_keys: StaticAccountKeysMeta {
                num_static_accounts: 1,
                offset: 2,
            },
            recent_blockhash_offset: 3,
            instructions: InstructionsMeta {
                num_instructions: 0,
                offset: 4,
            },
            address_table_lookup: AddressTableLookupMeta {
                num_address_table_lookups: 0,
                offset: 5,
                total_writable_lookup_accounts: 0,
                total_readonly_lookup_accounts: 0,
            },
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
    fn test_sanitize_dummy() {
        // The dummy metadata object should pass all sanitization checks.
        // Otherwise the tests which modify it are incorrect.
        let view = UnsanitizedTransactionView {
            data: &[][..],
            meta: dummy_metadata(),
        };
        assert!(sanitize(view).is_ok());
    }

    #[test]
    fn test_sanitize_multiple_transfers() {
        let transaction = multiple_transfers();
        let data = bincode::serialize(&transaction).unwrap();
        let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
        assert!(sanitize(view).is_ok());
    }

    #[test]
    fn test_sanitize_signatures() {
        // Too few signatures.
        {
            let mut meta = dummy_metadata();
            meta.signature.num_signatures = 0;
            let view = UnsanitizedTransactionView {
                data: &[][..],
                meta,
            };
            assert_eq!(
                sanitize_signatures(&view),
                Err(TransactionViewError::SanitizeError)
            );
        }

        // Too many signatures.
        {
            let mut meta = dummy_metadata();
            meta.signature.num_signatures = 2;
            let view = UnsanitizedTransactionView {
                data: &[][..],
                meta,
            };
            assert_eq!(
                sanitize_signatures(&view),
                Err(TransactionViewError::SanitizeError)
            );
        }

        // Not enough static accounts.
        {
            let mut meta = dummy_metadata();
            meta.signature.num_signatures = 2;
            meta.message_header.num_required_signatures = 2;
            meta.static_account_keys.num_static_accounts = 1;
            let view = UnsanitizedTransactionView {
                data: &[][..],
                meta,
            };
            assert_eq!(
                sanitize_signatures(&view),
                Err(TransactionViewError::SanitizeError)
            );
        }
    }

    #[test]
    fn test_sanitize_account_access() {
        // Overlap of signing and readonly non-signing accounts.
        {
            let mut meta = dummy_metadata();
            meta.message_header.num_readonly_unsigned_accounts = 2;
            meta.static_account_keys.num_static_accounts = 2;
            let view = UnsanitizedTransactionView {
                data: &[][..],
                meta,
            };
            assert_eq!(
                sanitize_account_access(&view),
                Err(TransactionViewError::SanitizeError)
            );
        }

        // Not enough writable accounts.
        {
            let mut meta = dummy_metadata();
            meta.message_header.num_readonly_signed_accounts = 1;
            meta.static_account_keys.num_static_accounts = 2;
            let view = UnsanitizedTransactionView {
                data: &[][..],
                meta,
            };
            assert_eq!(
                sanitize_account_access(&view),
                Err(TransactionViewError::SanitizeError)
            );
        }

        // Too many accounts.
        {
            let mut meta = dummy_metadata();
            meta.static_account_keys.num_static_accounts = 1;
            meta.address_table_lookup.total_writable_lookup_accounts = 200;
            meta.address_table_lookup.total_readonly_lookup_accounts = 200;
            let view = UnsanitizedTransactionView {
                data: &[][..],
                meta,
            };
            assert_eq!(
                sanitize_account_access(&view),
                Err(TransactionViewError::SanitizeError)
            );
        }
    }

    #[test]
    fn test_sanitize_instructions() {
        let transaction = multiple_transfers();
        let data = bincode::serialize(&transaction).unwrap();
        let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();

        let instruction_size =
            bincode::serialized_size(&transaction.message.instructions()[0]).unwrap();
        for instruction_index in 0..usize::from(view.num_instructions()) {
            let instruction_offset = usize::from(view.meta.instructions.offset)
                + instruction_index * instruction_size as usize;

            // Invalid program index.
            {
                let mut data = data.clone();
                data[instruction_offset] = 4;
                let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
                assert_eq!(
                    sanitize_instructions(&view),
                    Err(TransactionViewError::SanitizeError)
                );
            }

            // Program index is fee-payer.
            {
                let mut data = data.clone();
                data[instruction_offset] = 0;
                let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
                assert_eq!(
                    sanitize_instructions(&view),
                    Err(TransactionViewError::SanitizeError)
                );
            }

            // Invalid account index.
            {
                let mut data = data.clone();
                // one byte for program index, one byte for number of accounts
                data[instruction_offset + 2] = 7;
                let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
                assert_eq!(
                    sanitize_instructions(&view),
                    Err(TransactionViewError::SanitizeError)
                );
            }
        }
    }

    #[test]
    fn test_sanitize_address_table_lookups() {
        fn create_transaction(empty_index: usize) -> VersionedTransaction {
            let payer = Pubkey::new_unique();
            VersionedTransaction {
                signatures: vec![Signature::default()], // 1 signature to be valid.
                message: VersionedMessage::V0(v0::Message {
                    header: MessageHeader {
                        num_required_signatures: 1,
                        num_readonly_signed_accounts: 0,
                        num_readonly_unsigned_accounts: 0,
                    },
                    account_keys: vec![payer],
                    recent_blockhash: Hash::default(),
                    instructions: vec![],
                    address_table_lookups: vec![
                        MessageAddressTableLookup {
                            account_key: Pubkey::new_unique(),
                            writable_indexes: if empty_index == 0 { vec![] } else { vec![0, 1] },
                            readonly_indexes: vec![],
                        },
                        MessageAddressTableLookup {
                            account_key: Pubkey::new_unique(),
                            writable_indexes: if empty_index == 1 { vec![] } else { vec![0, 1] },
                            readonly_indexes: vec![],
                        },
                    ],
                }),
            }
        }

        for empty_index in 0..2 {
            let transaction = create_transaction(empty_index);
            assert_eq!(
                transaction.message.address_table_lookups().unwrap().len(),
                2
            );
            let data = bincode::serialize(&transaction).unwrap();
            let view = TransactionView::try_new_unsanitized(data.as_ref()).unwrap();
            assert_eq!(
                sanitize_address_table_lookups(&view),
                Err(TransactionViewError::SanitizeError)
            );
        }
    }
}
