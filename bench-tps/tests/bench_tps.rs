#![allow(clippy::arithmetic_side_effects)]

use {
    serial_test::serial,
    solana_bench_tps::{
        bench::{do_bench_tps, generate_and_fund_keypairs},
        cli::Config,
        send_batch::generate_durable_nonce_accounts,
    },
    solana_commitment_config::CommitmentConfig,
    solana_connection_cache::connection_cache::NewConnectionConfig,
    solana_core::validator::ValidatorConfig,
    solana_faucet::faucet::run_local_faucet_for_tests,
    solana_fee_calculator::FeeRateGovernor,
    solana_keypair::Keypair,
    solana_local_cluster::{
        cluster::Cluster,
        local_cluster::{ClusterConfig, LocalCluster},
        validator_configs::make_identical_validator_configs,
    },
    solana_net_utils::SocketAddrSpace,
    solana_quic_client::{QuicConfig, QuicConnectionManager, QuicPool},
    solana_rent::Rent,
    solana_rpc::rpc::JsonRpcConfig,
    solana_rpc_client::rpc_client::RpcClient,
    solana_signer::Signer,
    solana_test_validator::{TestValidator, TestValidatorGenesis},
    solana_tpu_client::tpu_client::{TpuClient, TpuClientConfig},
    std::{sync::Arc, time::Duration},
};

fn create_local_cluster_and_client() -> (
    LocalCluster,
    Arc<TpuClient<QuicPool, QuicConnectionManager, QuicConfig>>,
) {
    let faucet_keypair = Keypair::new();
    let faucet_pubkey = faucet_keypair.pubkey();
    let faucet_addr = run_local_faucet_for_tests(
        faucet_keypair,
        None, /* per_time_cap */
        0,    /* port */
    );

    const NUM_NODES: usize = 1;
    let cluster = LocalCluster::new(
        &mut ClusterConfig {
            node_stakes: vec![999_990; NUM_NODES],
            mint_lamports: 200_000_000,
            validator_configs: make_identical_validator_configs(
                &ValidatorConfig {
                    rpc_config: JsonRpcConfig {
                        faucet_addr: Some(faucet_addr),
                        ..JsonRpcConfig::default_for_test()
                    },
                    ..ValidatorConfig::default_for_test()
                },
                NUM_NODES,
            ),
            ..ClusterConfig::default()
        },
        SocketAddrSpace::Unspecified,
    );

    cluster.transfer(&cluster.funding_keypair, &faucet_pubkey, 100_000_000);

    let client = Arc::new(
        cluster
            .build_validator_tpu_quic_client(cluster.entry_point_info.pubkey())
            .unwrap_or_else(|err| {
                panic!("Could not create TpuClient with Quic Cache {err:?}");
            }),
    );

    (cluster, client)
}

fn create_test_validator_and_client() -> (
    TestValidator,
    Arc<TpuClient<QuicPool, QuicConnectionManager, QuicConfig>>,
) {
    let mint_keypair = Keypair::new();
    let mint_pubkey = mint_keypair.pubkey();
    let faucet_addr = run_local_faucet_for_tests(
        mint_keypair,
        None, /* per_time_cap */
        0,    /* port */
    );

    let test_validator = TestValidatorGenesis::default()
        .fee_rate_governor(FeeRateGovernor::new(0, 0))
        .rent(Rent {
            lamports_per_byte: 1,
            ..Rent::default()
        })
        .faucet_addr(Some(faucet_addr))
        .add_program(
            "spl_instruction_padding",
            spl_instruction_padding_interface::ID,
        )
        .start_with_mint_address(mint_pubkey, SocketAddrSpace::Unspecified)
        .expect("validator start failed");

    let rpc_client = Arc::new(RpcClient::new_with_commitment(
        test_validator.rpc_url(),
        CommitmentConfig::processed(),
    ));
    let websocket_url = test_validator.rpc_pubsub_url();

    (
        test_validator,
        Arc::new(
            TpuClient::new(
                "tpu_client_quic_bench_tps",
                rpc_client,
                &websocket_url,
                TpuClientConfig::default(),
                QuicConnectionManager::new_with_connection_config(QuicConfig::new().unwrap()),
            )
            .expect("Should build Quic Tpu Client."),
        ),
    )
}

fn run_bench_tps(client: Arc<TpuClient<QuicPool, QuicConnectionManager, QuicConfig>>) {
    let config = Config {
        tx_count: 100,
        duration: Duration::from_secs(5),
        ..Config::default()
    };
    let keypair_count = config.tx_count * config.keypair_multiplier;
    let lamports_per_account = 1000;
    let keypairs = generate_and_fund_keypairs(
        client.clone(),
        &config.id,
        keypair_count,
        lamports_per_account,
        false,
        false,
    )
    .unwrap();
    let nonce_keypairs = if config.use_durable_nonce {
        Some(generate_durable_nonce_accounts(client.clone(), &keypairs))
    } else {
        None
    };

    let tps = do_bench_tps(client, config, keypairs, nonce_keypairs);
    assert!(tps > 100, "TPS less than expected {tps}");
}

#[test]
#[serial]
fn test_bench_tps_local_cluster() {
    agave_logger::setup();
    let (local_cluster, client) = create_local_cluster_and_client();
    run_bench_tps(client);
    drop(local_cluster);
}

#[test]
#[serial]
fn test_bench_tps_test_validator() {
    agave_logger::setup();
    let (test_validator, client) = create_test_validator_and_client();
    run_bench_tps(client);
    drop(test_validator)
}
