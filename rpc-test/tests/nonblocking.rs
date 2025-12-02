use {
    solana_pubkey::Pubkey,
    solana_system_transaction as system_transaction,
    solana_test_validator::TestValidatorGenesis,
    std::sync::Arc,
    tokio::time::{sleep, Duration, Instant},
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_send_transaction_via_rpc() {
    let (test_validator, mint_keypair) = TestValidatorGenesis::default().start_async().await;

    let rpc_client = Arc::new(test_validator.get_async_rpc_client());

    let recent_blockhash = rpc_client.get_latest_blockhash().await.unwrap();
    let tx = system_transaction::transfer(
        &mint_keypair,
        &Pubkey::new_unique(),
        1_000_000,
        recent_blockhash,
    );

    rpc_client.send_transaction(&tx).await.unwrap();

    let timeout = Duration::from_secs(5);
    let now = Instant::now();
    let signatures = vec![tx.signatures[0]];
    loop {
        assert!(now.elapsed() < timeout);
        let statuses = rpc_client
            .get_signature_statuses(&signatures)
            .await
            .unwrap();
        if !statuses.value.is_empty() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
}
