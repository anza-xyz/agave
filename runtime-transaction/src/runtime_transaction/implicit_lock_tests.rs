use {
    super::*,
    agave_transaction_view::transaction_view::SanitizedTransactionView,
    solana_loader_v3_interface::get_program_data_address,
    solana_message::{MessageHeader, SimpleAddressLoader, VersionedMessage, v0},
    solana_transaction::{
        sanitized::{MessageHash, SanitizedTransaction},
        versioned::{VersionedTransaction, sanitized::SanitizedVersionedTransaction},
    },
    std::collections::HashSet,
};

// Both keys may be static or loaded. ProgramData must never also get an implicit lock.
#[test_case::test_matrix([false, true], [false, true], [false, true])]
fn test_implicit_locks_after_resolution(
    program_loaded: bool,
    programdata_loaded: bool,
    enable_implicit_read_locks: bool,
) {
    let payer = Pubkey::new_unique();
    let program = Pubkey::new_unique();
    let programdata = get_program_data_address(&program);
    let table = Pubkey::new_unique();
    let mut message = v0::Message {
        header: MessageHeader {
            num_required_signatures: 1,
            ..MessageHeader::default()
        },
        account_keys: vec![payer],
        ..v0::Message::default()
    };
    let mut loaded = v0::LoadedAddresses::default();
    for (key, in_table) in [(program, program_loaded), (programdata, programdata_loaded)] {
        if in_table {
            loaded.readonly.push(key);
        } else {
            message.account_keys.push(key);
            message.header.num_readonly_unsigned_accounts += 1;
        }
    }
    if !loaded.readonly.is_empty() {
        message
            .address_table_lookups
            .push(v0::MessageAddressTableLookup {
                account_key: table,
                writable_indexes: vec![],
                readonly_indexes: (0..loaded.readonly.len() as u8).collect(),
            });
    }
    let transaction = VersionedTransaction {
        signatures: vec![Signature::default()],
        message: VersionedMessage::V0(message),
    };
    let bytes = Bytes::from(wincode::serialize(&transaction).unwrap());
    let sdk = RuntimeTransaction::<SanitizedVersionedTransaction>::try_from(
        transaction,
        MessageHash::Compute,
        None,
    )
    .unwrap();
    let view = RuntimeTransaction::<SanitizedTransactionView<_>>::try_new(
        SanitizedTransactionView::try_new_sanitized(bytes).unwrap(),
        MessageHash::Compute,
        None,
    )
    .unwrap();
    for locks in [sdk.implicit_read_locks(), view.implicit_read_locks()] {
        assert!(locks.is_empty());
    }
    let mut sdk = RuntimeTransaction::<SanitizedTransaction>::try_from(
        sdk,
        SimpleAddressLoader::Enabled(loaded.clone()),
        &HashSet::new(),
    )
    .unwrap();
    let mut view = RuntimeTransaction::<ResolvedTransactionView<_>>::try_new(
        view,
        Some(loaded),
        &HashSet::new(),
    )
    .unwrap();
    assert!(sdk.implicit_read_locks().is_empty());
    assert!(view.implicit_read_locks().is_empty());
    if enable_implicit_read_locks {
        sdk.load_implicit_read_locks();
        view.load_implicit_read_locks();
    }
    for locks in [sdk.implicit_read_locks(), view.implicit_read_locks()] {
        if !enable_implicit_read_locks {
            assert!(locks.is_empty());
            continue;
        }
        // Explicit ProgramData replaces the derived read; every explicit key still derives a lock.
        assert_eq!(locks.len(), 2);
        assert!(locks.contains(&get_program_data_address(&payer)));
        assert!(locks.contains(&get_program_data_address(&programdata)));
        assert!(!locks.contains(&get_program_data_address(&table)));
    }
}
