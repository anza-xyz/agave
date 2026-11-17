use {
    solana_account::Account,
    solana_address_lookup_table_interface::state::{AddressLookupTable, LookupTableMeta},
    solana_instruction::{AccountMeta, Instruction},
    solana_message::{AddressLookupTableAccount, VersionedMessage, v0},
    solana_program_test::ProgramTest,
    solana_pubkey::Pubkey,
    solana_sdk_ids::{address_lookup_table, bpf_loader},
    solana_signer::Signer,
    solana_transaction::{Transaction, versioned::VersionedTransaction},
    solana_transaction_error::TransactionError,
    test_case::test_case,
};

#[tokio::test]
async fn test_add_bpf_program() {
    let program_id = Pubkey::new_unique();

    let mut program_test = ProgramTest::default();
    program_test.prefer_bpf(true);
    program_test.add_program("noop_program", program_id, None);

    let context = program_test.start_with_context().await;

    // Assert the program is a BPF Loader 2 program.
    let program_account = context
        .banks_client
        .get_account(program_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(program_account.owner, bpf_loader::id());

    // Invoke the program.
    let instruction = Instruction::new_with_bytes(program_id, &[], Vec::new());
    let transaction = Transaction::new_signed_with_payer(
        &[instruction],
        Some(&context.payer.pubkey()),
        &[&context.payer],
        context.last_blockhash,
    );
    context
        .banks_client
        .process_transaction(transaction)
        .await
        .unwrap();
}

#[test_case(64, None, true; "success with 64 accounts by default")]
#[test_case(65, None, false; "failure with 65 accounts by default")]
#[test_case(128, Some(128), true; "success with 128 accounts with runtime override")]
#[test_case(129, Some(128), false; "failure with 129 accounts with runtime override")]
#[test_case(32, Some(32), true; "success with 32 accounts with runtime override")]
#[test_case(33, Some(32), false; "failure with 33 accounts with runtime override")]
#[tokio::test]
async fn test_max_accounts(
    num_accounts: u8,
    account_lock_limit: Option<usize>,
    expect_success: bool,
) {
    let program_id = Pubkey::new_unique();

    let mut program_test = ProgramTest::default();

    program_test.prefer_bpf(true);
    program_test.add_program("noop_program", program_id, None);
    if let Some(limit) = account_lock_limit {
        program_test.set_transaction_account_lock_limit(limit);
    }

    // Subtract 2 to account for the program and fee payer
    let num_extra_accounts = num_accounts.checked_sub(2).unwrap();
    let account_metas = (0..num_extra_accounts)
        .map(|_| AccountMeta::new_readonly(Pubkey::new_unique(), false))
        .collect::<Vec<_>>();
    let lookup = AddressLookupTableAccount {
        key: Pubkey::new_unique(),
        addresses: account_metas.iter().map(|meta| meta.pubkey).collect(),
    };
    let data = AddressLookupTable {
        meta: LookupTableMeta::default(),
        addresses: lookup.addresses.clone().into(),
    }
    .serialize_for_tests()
    .unwrap();
    program_test.add_account(
        lookup.key,
        Account {
            lamports: 1_000_000_000,
            data,
            owner: address_lookup_table::id(),
            ..Account::default()
        },
    );
    let context = program_test.start_with_context().await;
    let instruction = Instruction::new_with_bytes(program_id, &[], account_metas);
    let message = v0::Message::try_compile(
        &context.payer.pubkey(),
        &[instruction],
        &[lookup],
        context.last_blockhash,
    )
    .unwrap();
    let transaction =
        VersionedTransaction::try_new(VersionedMessage::V0(message), &[&context.payer]).unwrap();

    // Invoke the program.
    if expect_success {
        context
            .banks_client
            .process_transaction_with_preflight(transaction)
            .await
            .unwrap();
    } else {
        assert_eq!(
            context
                .banks_client
                .process_transaction_with_preflight(transaction)
                .await
                .unwrap_err()
                .unwrap(),
            TransactionError::TooManyAccountLocks
        );
    }
}
