use {
    async_trait::async_trait,
    serde_json::{Value, json},
    solana_cli::{
        check_balance,
        cli::{CliCommand, CliConfig, process_command, request_and_confirm_airdrop},
    },
    solana_commitment_config::CommitmentConfig,
    solana_faucet::faucet::run_local_faucet_with_unique_port_for_tests,
    solana_keypair::{Keypair, keypair_from_seed},
    solana_net_utils::SocketAddrSpace,
    solana_rpc_client::{
        http_sender::HttpSender,
        nonblocking::rpc_client::RpcClient,
        rpc_client::RpcClientConfig,
        rpc_sender::{RpcSender, RpcTransportStats},
    },
    solana_rpc_client_api::{client_error::Result, request::RpcRequest},
    solana_test_validator::TestValidator,
    std::sync::Arc,
    test_case::test_case,
};

struct ValidatorInfoSender(HttpSender);

#[async_trait]
impl RpcSender for ValidatorInfoSender {
    async fn send(&self, request: RpcRequest, params: Value) -> Result<Value> {
        if request == RpcRequest::GetProgramAccounts {
            // This fresh validator has no validator-info accounts. Account listing is now
            // served by external RPCs, so provide that response and send everything else live.
            assert_eq!(params[0], solana_config_interface::id().to_string());
            return Ok(json!([]));
        }
        self.0.send(request, params).await
    }

    fn get_transport_stats(&self) -> RpcTransportStats {
        self.0.get_transport_stats()
    }

    fn url(&self) -> String {
        self.0.url()
    }
}

#[test_case(None; "base")]
#[test_case(Some(1_000_000); "with_compute_unit_price")]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_publish(compute_unit_price: Option<u64>) {
    agave_logger::setup();

    let mint_keypair = Keypair::new();
    let faucet_addr = run_local_faucet_with_unique_port_for_tests(mint_keypair.insecure_clone());
    let test_validator = TestValidator::async_start_with_config(
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
    config_validator.rpc_client = Some(Arc::new(RpcClient::new_sender(
        ValidatorInfoSender(HttpSender::new(test_validator.rpc_url())),
        RpcClientConfig::with_commitment(CommitmentConfig::processed()),
    )));

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

    config_validator.command = CliCommand::PublishValidatorInfo {
        validator_info: json!({ "name": "test" }),
        force_keybase: true,
        info_pubkey: None,
        compute_unit_price,
    };
    process_command(&config_validator).await.unwrap();
}
