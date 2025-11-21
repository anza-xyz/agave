use {
    solana_cli::{
        check_balance,
        cli::{process_command, request_and_confirm_airdrop, CliCommand, CliConfig},
        test_utils::check_ready,
    },
    solana_commitment_config::CommitmentConfig,
    solana_faucet::faucet::run_local_faucet_with_unique_port_for_tests,
    solana_fee_structure::FeeStructure,
    solana_keypair::Keypair,
    solana_native_token::LAMPORTS_PER_SOL,
    solana_net_utils::SocketAddrSpace,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_signer::Signer,
    solana_test_validator::TestValidator,
    std::time::Duration,
    test_case::test_case,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[test_case(false, None; "rpc_base")]
#[test_case(false, Some(1_000_000); "rpc_with_compute_unit_price")]
#[test_case(true, None; "tpu_base")]
#[test_case(true, Some(1_000_000); "tpu_with_compute_unit_price")]
async fn test_ping(use_tpu_client: bool, compute_unit_price: Option<u64>) {
    agave_logger::setup();
    let fee = FeeStructure::default().get_max_fee(1, 0);
    let mint_keypair = Keypair::new();
    let faucet_addr = run_local_faucet_with_unique_port_for_tests(mint_keypair.insecure_clone());
    let test_validator = TestValidator::async_with_custom_fees(
        &mint_keypair,
        fee,
        Some(faucet_addr),
        SocketAddrSpace::Unspecified,
    )
    .await;

    let rpc_client =
        RpcClient::new_with_commitment(test_validator.rpc_url(), CommitmentConfig::processed());

    let default_signer = Keypair::new();
    let signer_pubkey = default_signer.pubkey();

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    config.websocket_url = test_validator.rpc_pubsub_url();
    config.signers = vec![&default_signer];
    config.use_tpu_client = use_tpu_client;

    request_and_confirm_airdrop(&rpc_client, &config, &signer_pubkey, LAMPORTS_PER_SOL)
        .await
        .unwrap();
    check_balance!(LAMPORTS_PER_SOL, &rpc_client, &signer_pubkey);
    check_ready(&rpc_client).await;

    let count = 5;
    config.command = CliCommand::Ping {
        interval: Duration::from_secs(0),
        count: Some(count),
        timeout: Duration::from_secs(15),
        blockhash: None,
        print_timestamp: false,
        compute_unit_price,
    };
    process_command(&config).await.unwrap();
}
