use {
    assert_matches::assert_matches, solana_ed25519_program::new_ed25519_instruction_with_signature,
    solana_instruction::error::InstructionError, solana_keypair::Keypair,
    solana_precompile_error::PrecompileError, solana_program_test::*, solana_signer::Signer,
    solana_transaction::Transaction, solana_transaction_error::TransactionError,
};

#[tokio::test]
async fn test_success() {
    let mut context = ProgramTest::default().start_with_context().await;

    let client = &mut context.banks_client;
    let payer = &context.payer;
    let recent_blockhash = context.last_blockhash;

    let message_arr = b"hello";
    let keypair = Keypair::new();
    let signature = keypair.sign_message(message_arr);
    let pubkey = keypair.pubkey().to_bytes();
    let instruction =
        new_ed25519_instruction_with_signature(message_arr, signature.as_array(), &pubkey);

    let transaction = Transaction::new_signed_with_payer(
        &[instruction],
        Some(&payer.pubkey()),
        &[payer],
        recent_blockhash,
    );

    assert_matches!(client.process_transaction(transaction).await, Ok(()));
}

#[tokio::test]
async fn test_failure() {
    let mut context = ProgramTest::default().start_with_context().await;

    let client = &mut context.banks_client;
    let payer = &context.payer;
    let recent_blockhash = context.last_blockhash;

    let message_arr = b"hello";
    let keypair = Keypair::new();
    let signature = keypair.sign_message(message_arr);
    let pubkey = keypair.pubkey().to_bytes();
    let mut instruction =
        new_ed25519_instruction_with_signature(message_arr, signature.as_array(), &pubkey);

    instruction.data[0] += 1;

    let transaction = Transaction::new_signed_with_payer(
        &[instruction],
        Some(&payer.pubkey()),
        &[payer],
        recent_blockhash,
    );

    assert_matches!(
        client.process_transaction(transaction).await,
        Err(BanksClientError::TransactionError(
            TransactionError::InstructionError(0, InstructionError::Custom(3))
        ))
    );
    // this assert is for documenting the matched error code above
    assert_eq!(3, PrecompileError::InvalidDataOffsets as u32);
}
