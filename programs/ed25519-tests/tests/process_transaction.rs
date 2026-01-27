use {
    assert_matches::assert_matches,
    ed25519_dalek::{Signer as EdSigner, SigningKey},
    solana_ed25519_program::new_ed25519_instruction_with_signature,
    solana_instruction::error::InstructionError,
    solana_precompile_error::PrecompileError,
    solana_program_test::*,
    solana_signer::Signer,
    solana_transaction::Transaction,
    solana_transaction_error::TransactionError,
};

fn generate_keypair() -> SigningKey {
    use rand::RngCore;
    let mut seed = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);
    SigningKey::from_bytes(&seed)
}

#[tokio::test]
async fn test_success() {
    let mut context = ProgramTest::default().start_with_context().await;

    let client = &mut context.banks_client;
    let payer = &context.payer;
    let recent_blockhash = context.last_blockhash;

    let privkey = generate_keypair();
    let message_arr = b"hello";
    let signature = privkey.sign(message_arr).to_bytes();
    let pubkey = privkey.verifying_key().to_bytes();
    let instruction = new_ed25519_instruction_with_signature(message_arr, &signature, &pubkey);

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

    let privkey = generate_keypair();
    let message_arr = b"hello";
    let signature = privkey.sign(message_arr).to_bytes();
    let pubkey = privkey.verifying_key().to_bytes();
    let mut instruction = new_ed25519_instruction_with_signature(message_arr, &signature, &pubkey);

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
