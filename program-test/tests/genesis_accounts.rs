use {solana_account::Account, solana_program_test::ProgramTest, solana_pubkey::Pubkey};

#[tokio::test]
async fn genesis_accounts() {
    let my_genesis_accounts = [
        (
            Pubkey::new_unique(),
            Account::new(1, 0, &solana_system_interface::program::id()),
        ),
        (
            Pubkey::new_unique(),
            Account::new(1, 0, &solana_sdk_ids::config::id()),
        ),
        (
            Pubkey::new_unique(),
            Account::new(1, 0, &solana_sdk_ids::feature::id()),
        ),
        (
            Pubkey::new_unique(),
            Account::new(1, 0, &solana_stake_interface::program::id()),
        ),
    ];

    let mut program_test = ProgramTest::default();

    for (pubkey, account) in my_genesis_accounts.iter() {
        program_test.add_genesis_account(*pubkey, account.clone());
    }

    let context = program_test.start_with_context().await;

    // Verify the accounts are present.
    for (pubkey, account) in my_genesis_accounts.iter() {
        let fetched_account = context
            .banks_client
            .get_account(*pubkey)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched_account, *account);
    }
}
