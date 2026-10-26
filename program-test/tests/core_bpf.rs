use {
    solana_program_test::{ProgramTest, ProgramTestContext},
    solana_pubkey::Pubkey,
    solana_sdk_ids::bpf_loader_upgradeable,
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
    assert_bpf_program(&context, &solana_sdk_ids::stake::id()).await;
}
