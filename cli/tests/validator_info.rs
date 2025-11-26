use {
    serde_json::json,
    solana_cli::{
        check_balance,
        cli::{process_command, request_and_confirm_airdrop, CliCommand, CliConfig},
    },
    solana_commitment_config::CommitmentConfig,
    solana_faucet::faucet::run_local_faucet_with_unique_port_for_tests,
    solana_keypair::{keypair_from_seed, Keypair},
    solana_net_utils::SocketAddrSpace,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_test_validator::TestValidator,
    test_case::test_case,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[test_case(None; "base")]
#[test_case(Some(1_000_000); "with_compute_unit_price")]
async fn test_publish(compute_unit_price: Option<u64>) {
    agave_logger::setup();

    let mint_keypair = Keypair::new();
    let faucet_addr = run_local_faucet_with_unique_port_for_tests(mint_keypair.insecure_clone());
    let test_validator = TestValidator::async_with_no_fees(
        &mint_keypair,
        Some(faucet_addr),
        SocketAddrSpace::Unspecified,
    )
    .await;

    let rpc_client =
        RpcClient::new_with_commitment(test_validator.rpc_url(), CommitmentConfig::processed());

    let validator_keypair = keypair_from_seed(&[0u8; 32]).unwrap();
    let mut config_validator = CliConfig::recent_for_tests();
    config_validator.json_rpc_url = test_validator.rpc_url();
    config_validator.signers = vec![&validator_keypair];

    request_and_confirm_airdrop(
        &rpc_client,
        &config_validator,
        &config_validator.signers[0].pubkey(),
        100_000_000_000,
    )
    .await
    .unwrap();
    check_balance!(
        100_000_000_000,
        &rpc_client,
        &config_validator.signers[0].pubkey()
    );

    config_validator.command = CliCommand::SetValidatorInfo {
        validator_info: json!({ "name": "test" }),
        force_keybase: true,
        info_pubkey: None,
        compute_unit_price,
    };
    process_command(&config_validator).await.unwrap();
}
