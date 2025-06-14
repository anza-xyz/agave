use {
    assert_matches::assert_matches, ed25519_dalek::ed25519::signature::Signer as EdSigner,
    solana_ed25519_program::new_ed25519_instruction_with_signature,
    solana_instruction::error::InstructionError, solana_precompile_error::PrecompileError,
    solana_program_test::*, solana_signer::Signer, solana_transaction::Transaction,
    solana_transaction_error::TransactionError,
};

// Since ed25519_dalek is still using the old version of rand, this test
// copies the `generate` implementation at:
// https://docs.rs/ed25519-dalek/1.0.1/src/ed25519_dalek/secret.rs.html#167
fn generate_keypair() -> ed25519_dalek::Keypair {
    use rand::RngCore;
    let mut rng = rand::thread_rng();
    let mut seed = [0u8; ed25519_dalek::SECRET_KEY_LENGTH];
    rng.fill_bytes(&mut seed);
    let secret =
        ed25519_dalek::SecretKey::from_bytes(&seed[..ed25519_dalek::SECRET_KEY_LENGTH]).unwrap();
    let public = ed25519_dalek::PublicKey::from(&secret);
    ed25519_dalek::Keypair { secret, public }
}

#[tokio::test]
async fn test_success() {
    let context = ProgramTest::default().start_with_context().await;

    
    let payer = &context.payer;
    let recent_blockhash = context.bank.last_blockhash();

    let privkey = generate_keypair();
    let message_arr = b"hello";
    let signature = privkey.sign(message_arr).to_bytes();
    let pubkey = privkey.public.to_bytes();
    let instruction = new_ed25519_instruction_with_signature(message_arr, &signature, &pubkey);

    let transaction = Transaction::new_signed_with_payer(
        &[instruction],
        Some(&payer.pubkey()),
        &[payer],
        recent_blockhash,
    );

    assert_matches!(context.bank.process_transaction(&transaction), Ok(()));
}

#[tokio::test]
async fn test_failure() {
    let context = ProgramTest::default().start_with_context().await;

    
    let payer = &context.payer;
    let recent_blockhash = context.bank.last_blockhash();

    let privkey = generate_keypair();
    let message_arr = b"hello";
    let signature = privkey.sign(message_arr).to_bytes();
    let pubkey = privkey.public.to_bytes();
    let mut instruction = new_ed25519_instruction_with_signature(message_arr, &signature, &pubkey);

    instruction.data[0] += 1;

    let transaction = Transaction::new_signed_with_payer(
        &[instruction],
        Some(&payer.pubkey()),
        &[payer],
        recent_blockhash,
    );

    assert_matches!(
        context.bank.process_transaction(&transaction),
        Err(TransactionError::InstructionError(0, InstructionError::Custom(3)))
    );
    // this assert is for documenting the matched error code above
    assert_eq!(3, PrecompileError::InvalidDataOffsets as u32);
}
