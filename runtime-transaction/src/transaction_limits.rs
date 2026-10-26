//! Protocol limits checked before constructing a statically loaded runtime transaction.
//! Format validity is established separately by SDK / transaction-view sanitization.

use {
    agave_transaction_view::{
        transaction_data::TransactionData, transaction_view::SanitizedTransactionView,
    },
    solana_compute_budget::compute_budget_limits::{MAX_HEAP_FRAME_BYTES, MIN_HEAP_FRAME_BYTES},
    solana_message::v1,
    solana_pubkey::Pubkey,
    solana_svm_transaction::svm_transaction::SVMStaticTransaction,
    solana_transaction::versioned::TransactionVersion,
    solana_transaction_context::{MAX_ACCOUNTS_PER_INSTRUCTION, MAX_INSTRUCTION_TRACE_LENGTH},
    solana_transaction_error::{TransactionError, TransactionResult},
    std::collections::HashSet,
};

/// Maximum serialized transaction size for legacy and v0 transactions, in bytes.
const MAX_LEGACY_AND_V0_TRANSACTION_SIZE: usize = 1232;
const MAX_V1_SIGNATURES: usize = 12;
const MAX_V1_ACCOUNTS: usize = 64;

pub(crate) fn validate_unique_account_keys<'a>(
    account_keys: impl IntoIterator<Item = &'a Pubkey>,
) -> TransactionResult<()> {
    let mut seen = HashSet::new();
    if account_keys.into_iter().any(|key| !seen.insert(key)) {
        Err(TransactionError::AccountLoadedTwice)
    } else {
        Ok(())
    }
}

pub(crate) fn validate_transaction_limits(
    transaction: &impl SVMStaticTransaction,
    serialized_size: u64,
) -> TransactionResult<()> {
    let (max_transaction_size, max_signatures, max_accounts) = match transaction.version() {
        TransactionVersion::Legacy(_) | TransactionVersion::Number(0) => {
            (MAX_LEGACY_AND_V0_TRANSACTION_SIZE, None, None)
        }
        TransactionVersion::Number(1) => (
            v1::MAX_TRANSACTION_SIZE,
            Some(MAX_V1_SIGNATURES),
            Some(MAX_V1_ACCOUNTS),
        ),
        TransactionVersion::Number(_) => return Err(TransactionError::SanitizeFailure),
    };
    if serialized_size > max_transaction_size as u64
        || max_signatures.is_some_and(|max| transaction.signatures().len() > max)
        || max_accounts.is_some_and(|max| transaction.static_account_keys().len() > max)
        || transaction.num_instructions() > MAX_INSTRUCTION_TRACE_LENGTH
    {
        return Err(TransactionError::SanitizeFailure);
    }

    for instruction in transaction.instructions_iter() {
        if instruction.accounts.len() > MAX_ACCOUNTS_PER_INSTRUCTION {
            return Err(TransactionError::SanitizeFailure);
        }
    }
    Ok(())
}

// SVMStaticTransaction does not expose V1 configuration, so constructors check
// configuration separately using their representation's config accessor.
pub(crate) fn validate_transaction_config(config: &v1::TransactionConfig) -> TransactionResult<()> {
    validate_heap_size(config.heap_size)
}

// TransactionConfigView is not publicly exported by transaction-view, so accept
// the enclosing view and access its config without materializing an SDK config.
pub(crate) fn validate_transaction_config_view(
    transaction: &SanitizedTransactionView<impl TransactionData>,
) -> TransactionResult<()> {
    validate_heap_size(
        transaction
            .transaction_config()
            .and_then(|config| config.requested_heap_size()),
    )
}

fn validate_heap_size(requested_heap_size: Option<u32>) -> TransactionResult<()> {
    if let Some(heap_size) = requested_heap_size
        && (!(MIN_HEAP_FRAME_BYTES..=MAX_HEAP_FRAME_BYTES).contains(&heap_size)
            || !heap_size.is_multiple_of(1024))
    {
        return Err(TransactionError::SanitizeFailure);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::runtime_transaction::RuntimeTransaction,
        agave_transaction_view::{
            resolved_transaction_view::ResolvedTransactionView,
            transaction_version::TransactionVersion, transaction_view::SanitizedTransactionView,
        },
        solana_hash::Hash,
        solana_message::{
            Message, MessageHeader, SimpleAddressLoader, VersionedMessage,
            compiled_instruction::CompiledInstruction,
            v0::{self, LoadedAddresses, MessageAddressTableLookup},
        },
        solana_pubkey::Pubkey,
        solana_signature::Signature,
        solana_transaction::{
            sanitized::{MessageHash, SanitizedTransaction},
            versioned::{VersionedTransaction, sanitized::SanitizedVersionedTransaction},
        },
    };

    fn transaction(
        version: TransactionVersion,
        instructions: Vec<CompiledInstruction>,
    ) -> VersionedTransaction {
        let header = MessageHeader {
            num_required_signatures: 1,
            num_readonly_signed_accounts: 0,
            num_readonly_unsigned_accounts: 1,
        };
        let account_keys = vec![Pubkey::new_unique(), Pubkey::new_unique()];
        let message = match version {
            TransactionVersion::Legacy => VersionedMessage::Legacy(Message {
                header,
                account_keys,
                recent_blockhash: Hash::default(),
                instructions,
            }),
            TransactionVersion::V0 => VersionedMessage::V0(v0::Message {
                header,
                account_keys,
                recent_blockhash: Hash::default(),
                instructions,
                address_table_lookups: vec![],
            }),
            TransactionVersion::V1 => VersionedMessage::V1(v1::Message {
                header,
                account_keys,
                lifetime_specifier: Hash::default(),
                instructions,
                config: v1::TransactionConfig::default(),
            }),
        };
        VersionedTransaction {
            signatures: vec![Signature::default()],
            message,
        }
    }

    fn instruction(num_accounts: usize, data_len: usize) -> CompiledInstruction {
        CompiledInstruction {
            program_id_index: 1,
            accounts: vec![0; num_accounts],
            data: vec![0; data_len],
        }
    }

    fn assert_sdk_constructor(transaction: VersionedTransaction, valid: bool) {
        let result = RuntimeTransaction::<SanitizedVersionedTransaction>::try_from(
            transaction,
            MessageHash::Compute,
            None,
        );
        assert_eq!(
            result.err(),
            (!valid).then_some(TransactionError::SanitizeFailure)
        );
    }

    #[test]
    fn test_duplicate_static_accounts_rejected_by_resolved_constructors() {
        for version in [
            TransactionVersion::Legacy,
            TransactionVersion::V0,
            TransactionVersion::V1,
        ] {
            let mut tx = transaction(version, vec![]);
            match &mut tx.message {
                VersionedMessage::Legacy(message) => {
                    message.account_keys[1] = message.account_keys[0]
                }
                VersionedMessage::V0(message) => message.account_keys[1] = message.account_keys[0],
                VersionedMessage::V1(message) => message.account_keys[1] = message.account_keys[0],
            }

            let bytes = wincode::serialize(&tx).unwrap();
            if !matches!(version, TransactionVersion::V1) {
                // The currently pinned SDK still rejects duplicate V1 keys during sanitization.
                let static_tx = RuntimeTransaction::<SanitizedVersionedTransaction>::try_from(
                    tx,
                    MessageHash::Compute,
                    None,
                )
                .unwrap();
                let address_loader = if matches!(version, TransactionVersion::V0) {
                    SimpleAddressLoader::Enabled(LoadedAddresses::default())
                } else {
                    SimpleAddressLoader::Disabled
                };
                assert_eq!(
                    RuntimeTransaction::<SanitizedTransaction>::try_from(
                        static_tx,
                        address_loader,
                        &HashSet::new(),
                    )
                    .err(),
                    Some(TransactionError::AccountLoadedTwice),
                );
            }

            let view = SanitizedTransactionView::try_new_sanitized(&bytes[..]).unwrap();
            let static_tx = RuntimeTransaction::<SanitizedTransactionView<_>>::try_new(
                view,
                MessageHash::Compute,
                None,
            )
            .unwrap();
            let loaded_addresses =
                matches!(version, TransactionVersion::V0).then(LoadedAddresses::default);
            assert_eq!(
                RuntimeTransaction::<ResolvedTransactionView<_>>::try_new(
                    static_tx,
                    loaded_addresses,
                    &HashSet::new(),
                )
                .err(),
                Some(TransactionError::AccountLoadedTwice),
            );
        }
    }

    #[test]
    fn test_duplicate_loaded_account_rejected_by_resolved_constructors() {
        let mut tx = transaction(TransactionVersion::V0, vec![]);
        let VersionedMessage::V0(message) = &mut tx.message else {
            unreachable!();
        };
        let duplicate_key = message.account_keys[0];
        message.address_table_lookups = vec![MessageAddressTableLookup {
            account_key: Pubkey::new_unique(),
            writable_indexes: vec![],
            readonly_indexes: vec![0],
        }];
        let loaded_addresses = LoadedAddresses {
            writable: vec![],
            readonly: vec![duplicate_key],
        };
        let bytes = wincode::serialize(&tx).unwrap();
        let sdk_tx = RuntimeTransaction::<SanitizedVersionedTransaction>::try_from(
            tx,
            MessageHash::Compute,
            None,
        )
        .unwrap();
        assert_eq!(
            RuntimeTransaction::<SanitizedTransaction>::try_from(
                sdk_tx,
                SimpleAddressLoader::Enabled(loaded_addresses.clone()),
                &HashSet::new(),
            )
            .err(),
            Some(TransactionError::AccountLoadedTwice),
        );

        let view = SanitizedTransactionView::try_new_sanitized(&bytes[..]).unwrap();
        let static_tx = RuntimeTransaction::<SanitizedTransactionView<_>>::try_new(
            view,
            MessageHash::Compute,
            None,
        )
        .unwrap();
        assert_eq!(
            RuntimeTransaction::<ResolvedTransactionView<_>>::try_new(
                static_tx,
                Some(loaded_addresses),
                &HashSet::new(),
            )
            .err(),
            Some(TransactionError::AccountLoadedTwice),
        );
    }

    #[test]
    fn test_instruction_limit_boundaries() {
        for version in [
            TransactionVersion::Legacy,
            TransactionVersion::V0,
            TransactionVersion::V1,
        ] {
            for (instructions, accounts, valid) in [
                (64, 0, true),
                (65, 0, false),
                (1, 255, true),
                (1, 256, false),
            ] {
                let tx = transaction(version, vec![instruction(accounts, 0); instructions]);
                // V1 cannot encode more than 255 accounts per instruction.
                match version {
                    TransactionVersion::V1 if accounts > 255 => {}
                    TransactionVersion::Legacy
                    | TransactionVersion::V0
                    | TransactionVersion::V1 => {
                        assert_view_constructors(&wincode::serialize(&tx).unwrap(), valid);
                    }
                }
                assert_sdk_constructor(tx, valid);
            }
        }
    }

    #[test]
    fn test_v1_signature_and_account_limits() {
        // Upstream SDK sanitization still rejects the over-limit cases first.
        // These constructor tests continue to apply when those checks move here.
        for (signatures, accounts, valid) in [(12, 64, true), (13, 64, false), (12, 65, false)] {
            let mut tx = transaction(TransactionVersion::V1, vec![]);
            let VersionedMessage::V1(message) = &mut tx.message else {
                unreachable!();
            };
            message.header.num_required_signatures = signatures;
            message.account_keys = (0..accounts).map(|_| Pubkey::new_unique()).collect();
            tx.signatures = vec![Signature::default(); usize::from(signatures)];
            assert_sdk_constructor(tx, valid);
        }
    }

    #[test]
    fn test_transaction_config_heap_boundaries() {
        for (heap_size, valid) in [
            (None, true),
            (Some(MIN_HEAP_FRAME_BYTES), true),
            (Some(MAX_HEAP_FRAME_BYTES), true),
            (Some(0), false),
            (Some(MIN_HEAP_FRAME_BYTES - 1024), false),
            (Some(MAX_HEAP_FRAME_BYTES + 1024), false),
            (Some(MIN_HEAP_FRAME_BYTES + 1), false),
        ] {
            let config = v1::TransactionConfig {
                heap_size,
                priority_fee: Some(u64::MAX),
                compute_unit_limit: Some(u32::MAX),
                loaded_accounts_data_size_limit: Some(u32::MAX),
            };
            assert_eq!(
                validate_transaction_config(&config).err(),
                (!valid).then_some(TransactionError::SanitizeFailure),
            );
        }
    }

    #[test]
    fn test_sdk_transaction_size_limits() {
        for version in [
            TransactionVersion::Legacy,
            TransactionVersion::V0,
            TransactionVersion::V1,
        ] {
            let max_size = if matches!(version, TransactionVersion::V1) {
                v1::MAX_TRANSACTION_SIZE
            } else {
                MAX_LEGACY_AND_V0_TRANSACTION_SIZE
            };
            // Start with a two-byte short-u16 data length so padding does not
            // change the legacy/v0 length-prefix size.
            let tx = transaction(version, vec![instruction(0, 128)]);
            let padding = max_size - wincode::serialize(&tx).unwrap().len();
            for excess in [0, 1] {
                let tx = transaction(version, vec![instruction(0, 128 + padding + excess)]);
                let bytes = wincode::serialize(&tx).unwrap();
                assert_eq!(bytes.len(), max_size + excess);
                assert_sdk_constructor(tx, excess == 0);
                if excess == 0 {
                    assert_view_constructors(&bytes, true);
                }
            }
        }
    }

    fn assert_view_constructors(bytes: &[u8], valid: bool) {
        let view = SanitizedTransactionView::try_new_sanitized(bytes).unwrap();
        let borrowed = RuntimeTransaction::<&SanitizedTransactionView<_>>::try_new(
            &view,
            MessageHash::Compute,
            None,
        );
        assert_eq!(
            borrowed.err(),
            (!valid).then_some(TransactionError::SanitizeFailure)
        );
        let owned = RuntimeTransaction::<SanitizedTransactionView<_>>::try_new(
            view,
            MessageHash::Compute,
            None,
        );
        assert_eq!(
            owned.err(),
            (!valid).then_some(TransactionError::SanitizeFailure)
        );
    }

    #[test]
    fn test_view_constructors_reject_invalid_heap() {
        for heap in [MIN_HEAP_FRAME_BYTES - 1024, MAX_HEAP_FRAME_BYTES + 1024] {
            let mut tx = transaction(TransactionVersion::V1, vec![]);
            let VersionedMessage::V1(message) = &mut tx.message else {
                unreachable!()
            };
            message.config.heap_size = Some(heap);
            assert_view_constructors(&wincode::serialize(&tx).unwrap(), false);
        }
    }
}
