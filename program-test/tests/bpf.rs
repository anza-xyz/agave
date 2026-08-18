use {
    solana_instruction::{AccountMeta, Instruction},
    solana_program_test::ProgramTest,
    solana_pubkey::Pubkey,
    solana_sdk_ids::bpf_loader,
    solana_signer::Signer,
    solana_transaction::Transaction,
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

#[test_case(64, None, true; "success at the default account lock limit")]
#[test_case(65, None, false; "failure above the default account lock limit")]
#[test_case(128, Some(128), true; "success at an overridden account lock limit")]
#[test_case(129, Some(128), false; "failure above an overridden account lock limit")]
#[tokio::test]
async fn test_max_accounts(
    num_accounts: u8,
    transaction_account_lock_limit: Option<usize>,
    expect_success: bool,
) {
    let program_id = Pubkey::new_unique();

    let mut program_test = ProgramTest::default();

    program_test.prefer_bpf(true);
    program_test.add_program("noop_program", program_id, None);
    if let Some(transaction_account_lock_limit) = transaction_account_lock_limit {
        program_test.set_transaction_account_lock_limit(transaction_account_lock_limit);
    }

    let context = program_test.start_with_context().await;

    // Subtract 2 to account for the program and fee payer
    let num_extra_accounts = num_accounts.checked_sub(2).unwrap();
    let account_metas = (0..num_extra_accounts)
        .map(|_| AccountMeta::new_readonly(Pubkey::new_unique(), false))
        .collect::<Vec<_>>();
    let instruction = Instruction::new_with_bytes(program_id, &[], account_metas);
    let transaction = Transaction::new_signed_with_payer(
        &[instruction],
        Some(&context.payer.pubkey()),
        &[&context.payer],
        context.last_blockhash,
    );

    // Invoke the program.
    if expect_success {
        context
            .banks_client
            .process_transaction(transaction)
            .await
            .unwrap();
    } else {
        context
            .banks_client
            .process_transaction(transaction)
            .await
            .unwrap_err();
    }
}
