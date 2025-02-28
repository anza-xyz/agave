use {
    solana_program_test::{ProgramTest, ProgramTestContext},
    solana_sdk::{
        bpf_loader_upgradeable, instruction::Instruction, pubkey::Pubkey, signature::Signer,
        transaction::Transaction,
    },
};

async fn assert_bpf_program(context: &ProgramTestContext, program_id: &Pubkey) {
    let program_account = context
        .banks_client
        .get_account(*program_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(program_account.owner, bpf_loader_upgradeable::id());
    assert!(program_account.executable);
}

#[tokio::test]
async fn test_vended_core_bpf_programs() {
    let program_test = ProgramTest::default();
    let context = program_test.start_with_context().await;

    assert_bpf_program(&context, &solana_sdk_ids::address_lookup_table::id()).await;
    assert_bpf_program(&context, &solana_sdk_ids::config::id()).await;
    assert_bpf_program(&context, &solana_sdk_ids::feature::id()).await;
}

#[tokio::test]
async fn test_add_core_bpf_program_manually() {
    // Core BPF program: Stake.
    let program_id = solana_sdk_ids::stake::id();

    let mut program_test = ProgramTest::default();
    program_test.add_upgradeable_program_to_genesis("noop_program", &program_id);

    let context = program_test.start_with_context().await;

    // Assert the program is a BPF Loader Upgradeable program.
    assert_bpf_program(&context, &program_id).await;

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
