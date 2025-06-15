use {
    solana_account::ReadableAccount,
    solana_keypair::Keypair,
    solana_loader_v3_interface::state::UpgradeableLoaderState,
    solana_message::{v0::Message, VersionedMessage},
    solana_program_test::ProgramTest,
    solana_pubkey::Pubkey,
    solana_sdk_ids::bpf_loader_upgradeable,
    solana_signer::Signer,
    solana_system_interface::instruction as system_instruction,
    solana_transaction::{versioned::VersionedTransaction, Transaction},
};

#[tokio::test]
async fn test_bpf_loader_upgradeable_present() {
    // Arrange
    let (bank, payer) = ProgramTest::default().start().await;

    let buffer_keypair = Keypair::new();
    let upgrade_authority_keypair = Keypair::new();

    let rent = &bank.rent_collector().rent;
    let buffer_rent = rent.minimum_balance(UpgradeableLoaderState::size_of_programdata(1));

    let create_buffer_instructions = solana_loader_v3_interface::instruction::create_buffer(
        &payer.pubkey(),
        &buffer_keypair.pubkey(),
        &upgrade_authority_keypair.pubkey(),
        buffer_rent,
        1,
    )
    .unwrap();

    let mut transaction =
        Transaction::new_with_payer(&create_buffer_instructions[..], Some(&payer.pubkey()));
    transaction.sign(&[&payer, &buffer_keypair], bank.last_blockhash());

    // Act
    bank.process_transaction(&transaction).unwrap();

    // Assert
    let buffer_account = bank.get_account(&buffer_keypair.pubkey()).unwrap();

    assert_eq!(*buffer_account.owner(), bpf_loader_upgradeable::id());
}

#[tokio::test]
async fn versioned_transaction() {
    let program_test = ProgramTest::default();
    let context = program_test.start_with_context().await;

    let program_id = Pubkey::new_unique();
    let account = Keypair::new();
    let rent = &context.bank.rent_collector().rent;
    let space = 82;
    let transaction = VersionedTransaction::try_new(
        VersionedMessage::V0(
            Message::try_compile(
                &context.payer.pubkey(),
                &[system_instruction::create_account(
                    &context.payer.pubkey(),
                    &account.pubkey(),
                    rent.minimum_balance(space),
                    space as u64,
                    &program_id,
                )],
                &[],
                context.bank.last_blockhash(),
            )
            .unwrap(),
        ),
        &[&context.payer, &account],
    )
    .unwrap();

    context
        .bank
        .process_transaction_with_metadata(transaction)
        .unwrap()
        .status
        .unwrap();
}
