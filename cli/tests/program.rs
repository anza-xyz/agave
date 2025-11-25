#![allow(clippy::arithmetic_side_effects)]
// REMOVE once https://github.com/rust-lang/rust-clippy/issues/11153 is fixed
#![allow(clippy::items_after_test_module)]

use {
    agave_feature_set::enable_alt_bn128_syscall,
    assert_matches::assert_matches,
    serde_json::Value,
    solana_account::{state_traits::StateMut, ReadableAccount},
    solana_borsh::v1::try_from_slice_unchecked,
    solana_cli::{
        cli::{process_command, CliCommand, CliConfig},
        program::{ProgramCliCommand, CLOSE_PROGRAM_WARNING},
        program_v4::{AdditionalCliConfig, ProgramV4CliCommand},
        test_utils::wait_n_slots,
    },
    solana_cli_output::{parse_sign_only_reply_string, OutputFormat},
    solana_client::rpc_config::RpcSendTransactionConfig,
    solana_commitment_config::CommitmentConfig,
    solana_compute_budget_interface::ComputeBudgetInstruction,
    solana_faucet::faucet::run_local_faucet_with_unique_port_for_tests,
    solana_fee_calculator::FeeRateGovernor,
    solana_keypair::Keypair,
    solana_loader_v3_interface::state::UpgradeableLoaderState,
    solana_net_utils::SocketAddrSpace,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_rpc::rpc::JsonRpcConfig,
    solana_rpc_client::{
        nonblocking::rpc_client::RpcClient, rpc_client::GetConfirmedSignaturesForAddress2Config,
    },
    solana_rpc_client_api::config::RpcTransactionConfig,
    solana_rpc_client_nonce_utils::nonblocking::blockhash_query::BlockhashQuery,
    solana_sdk_ids::{bpf_loader_upgradeable, compute_budget, loader_v4},
    solana_signature::Signature,
    solana_signer::{null_signer::NullSigner, Signer},
    solana_system_interface::program as system_program,
    solana_test_validator::TestValidatorGenesis,
    solana_transaction::Transaction,
    solana_transaction_status::UiTransactionEncoding,
    std::{
        env,
        fs::File,
        io::{Read, Seek, SeekFrom},
        path::{Path, PathBuf},
        str::FromStr,
        sync::Arc,
    },
    test_case::test_case,
};

fn test_validator_genesis(mint_keypair: &Keypair) -> TestValidatorGenesis {
    let mut genesis = TestValidatorGenesis::default();
    genesis
        .fee_rate_governor(FeeRateGovernor::new(0, 0))
        .rent(Rent {
            lamports_per_byte_year: 1,
            exemption_threshold: 1.0,
            ..Rent::default()
        })
        .faucet_addr(Some(run_local_faucet_with_unique_port_for_tests(
            mint_keypair.insecure_clone(),
        )));
    genesis
}

fn setup_rpc_client(config: &mut CliConfig) -> Arc<RpcClient> {
    let rpc_client = Arc::new(RpcClient::new_with_timeouts_and_commitment(
        config.json_rpc_url.to_string(),
        config.rpc_timeout,
        config.commitment,
        config.confirm_transaction_initial_timeout,
    ));
    config.rpc_client = Some(rpc_client.clone());
    rpc_client
}

async fn expect_command_failure(
    config: &CliConfig<'_>,
    should_fail_because: &str,
    error_expected: &str,
) {
    let error_actual = process_command(config)
        .await
        .expect_err(should_fail_because);
    let error_actual = error_actual.to_string();
    assert!(
        error_expected == error_actual,
        "Command failed as expected, but with an unexpected error. Expected: {error_expected}, \
         Actual:   {error_actual}",
    );
}

async fn expect_account_absent(rpc_client: &RpcClient, pubkey: Pubkey, absent_because: &str) {
    let error_actual = rpc_client
        .get_account(&pubkey)
        .await
        .expect_err(absent_because);
    let error_actual = error_actual.to_string();
    assert!(
        format!("AccountNotFound: pubkey={pubkey}") == error_actual,
        "Failed to retrieve an account details. Expected account to be absent, but got a \
         different error: {error_actual}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cli_program_deploy_non_upgradeable() {
    agave_logger::setup();

    let mut noop_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    noop_path.push("tests");
    noop_path.push("fixtures");
    noop_path.push("noop");
    noop_path.set_extension("so");

    let mint_keypair = Keypair::new();
    let test_validator = test_validator_genesis(&mint_keypair)
        .start_async_with_mint_address(&mint_keypair, SocketAddrSpace::Unspecified)
        .await
        .expect("validator start failed");

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    let rpc_client = setup_rpc_client(&mut config);

    let mut file = File::open(noop_path.to_str().unwrap()).unwrap();
    let mut program_data = Vec::new();
    file.read_to_end(&mut program_data).unwrap();
    let minimum_balance_for_programdata = rpc_client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_programdata(
            program_data.len(),
        ))
        .await
        .unwrap();
    let minimum_balance_for_program = rpc_client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_program())
        .await
        .unwrap();

    let keypair = Keypair::new();
    config.signers = vec![&keypair];
    config.command = CliCommand::Airdrop {
        pubkey: None,
        lamports: 4 * minimum_balance_for_programdata, // min balance for rent exemption for three programs + leftover for tx processing
    };
    process_command(&config).await.unwrap();

    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: None,
        program_pubkey: None,
        buffer_signer_index: None,
        buffer_pubkey: None,
        upgrade_authority_signer_index: 0,
        is_final: true,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: true,
        use_rpc: false,
        skip_feature_verification: true,
    });
    config.output_format = OutputFormat::JsonCompact;
    let response = process_command(&config).await;
    let json: Value = serde_json::from_str(&response.unwrap()).unwrap();
    let program_id_str = json
        .as_object()
        .unwrap()
        .get("programId")
        .unwrap()
        .as_str()
        .unwrap();
    let program_id = Pubkey::from_str(program_id_str).unwrap();
    let account0 = rpc_client.get_account(&program_id).await.unwrap();
    assert_eq!(account0.lamports, minimum_balance_for_program);
    assert_eq!(account0.owner, bpf_loader_upgradeable::id());
    assert!(account0.executable);

    let (programdata_pubkey, _) =
        Pubkey::find_program_address(&[program_id.as_ref()], &bpf_loader_upgradeable::id());
    let programdata_account = rpc_client.get_account(&programdata_pubkey).await.unwrap();
    assert_eq!(
        programdata_account.lamports,
        minimum_balance_for_programdata
    );
    assert_eq!(programdata_account.owner, bpf_loader_upgradeable::id());
    assert!(!programdata_account.executable);
    assert_eq!(
        programdata_account.data[UpgradeableLoaderState::size_of_programdata_metadata()..],
        program_data[..]
    );

    // Test custom address
    let custom_address_keypair = Keypair::new();
    config.signers = vec![&keypair, &custom_address_keypair];
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: Some(1),
        program_pubkey: None,
        buffer_signer_index: None,
        buffer_pubkey: None,
        upgrade_authority_signer_index: 0,
        is_final: true,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: true,
        use_rpc: false,
        skip_feature_verification: true,
    });
    process_command(&config).await.unwrap();
    let account1 = rpc_client
        .get_account(&custom_address_keypair.pubkey())
        .await
        .unwrap();
    assert_eq!(account1.lamports, minimum_balance_for_program);
    assert_eq!(account1.owner, bpf_loader_upgradeable::id());
    assert!(account1.executable);
    let (programdata_pubkey, _) = Pubkey::find_program_address(
        &[custom_address_keypair.pubkey().as_ref()],
        &bpf_loader_upgradeable::id(),
    );
    let programdata_account = rpc_client.get_account(&programdata_pubkey).await.unwrap();
    assert_eq!(
        programdata_account.lamports,
        minimum_balance_for_programdata
    );
    assert_eq!(programdata_account.owner, bpf_loader_upgradeable::id());
    assert!(!programdata_account.executable);
    assert_eq!(
        programdata_account.data[UpgradeableLoaderState::size_of_programdata_metadata()..],
        program_data[..]
    );

    expect_command_failure(
        &config,
        "Program can not be deployed at the same address twice",
        &format!(
            "Program {} is no longer upgradeable",
            custom_address_keypair.pubkey()
        ),
    )
    .await;

    // Attempt to deploy to account with excess balance
    let custom_address_keypair = Keypair::new();
    config.signers = vec![&custom_address_keypair];
    config.command = CliCommand::Airdrop {
        pubkey: None,
        // Anything over minimum_balance_for_programdata should trigger an error.
        lamports: 2 * minimum_balance_for_programdata,
    };
    process_command(&config).await.unwrap();
    config.signers = vec![&keypair, &custom_address_keypair];
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: Some(1),
        program_pubkey: None,
        buffer_signer_index: None,
        buffer_pubkey: None,
        upgrade_authority_signer_index: 0,
        is_final: true,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: true,
        use_rpc: false,
        skip_feature_verification: true,
    });
    expect_command_failure(
        &config,
        "The CLI blocks deployments into accounts that hold more than the necessary amount of SOL",
        &format!(
            "Account {} is not an upgradeable program or already in use",
            custom_address_keypair.pubkey()
        ),
    )
    .await;

    // Use forcing parameter to deploy to account with excess balance
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: Some(1),
        program_pubkey: None,
        buffer_signer_index: None,
        buffer_pubkey: None,
        upgrade_authority_signer_index: 0,
        is_final: true,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: true,
        use_rpc: false,
        skip_feature_verification: true,
    });
    expect_command_failure(
        &config,
        "The program is non-upgradable, so even if we skip the CLI account balance check, the \
         upgrade still fails",
        &format!(
            "Account {} is not an upgradeable program or already in use",
            custom_address_keypair.pubkey()
        ),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cli_program_deploy_no_authority() {
    agave_logger::setup();

    let mut noop_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    noop_path.push("tests");
    noop_path.push("fixtures");
    noop_path.push("noop");
    noop_path.set_extension("so");

    let mint_keypair = Keypair::new();
    let test_validator = test_validator_genesis(&mint_keypair)
        .start_async_with_mint_address(&mint_keypair, SocketAddrSpace::Unspecified)
        .await
        .expect("validator start failed");

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    let rpc_client = setup_rpc_client(&mut config);

    let mut file = File::open(noop_path.to_str().unwrap()).unwrap();
    let mut program_data = Vec::new();
    file.read_to_end(&mut program_data).unwrap();
    let max_len = program_data.len();
    let minimum_balance_for_programdata = rpc_client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_programdata(
            max_len,
        ))
        .await
        .unwrap();
    let minimum_balance_for_program = rpc_client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_program())
        .await
        .unwrap();
    let upgrade_authority = Keypair::new();

    let keypair = Keypair::new();
    config.command = CliCommand::Airdrop {
        pubkey: None,
        lamports: 100 * minimum_balance_for_programdata + minimum_balance_for_program,
    };
    config.signers = vec![&keypair];
    process_command(&config).await.unwrap();

    // Deploy a program
    config.signers = vec![&keypair, &upgrade_authority];
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: None,
        program_pubkey: None,
        buffer_signer_index: None,
        buffer_pubkey: None,
        upgrade_authority_signer_index: 1,
        is_final: true,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: true,
        use_rpc: false,
        skip_feature_verification: true,
    });
    config.output_format = OutputFormat::JsonCompact;
    let response = process_command(&config).await;
    let json: Value = serde_json::from_str(&response.unwrap()).unwrap();
    let program_id_str = json
        .as_object()
        .unwrap()
        .get("programId")
        .unwrap()
        .as_str()
        .unwrap();
    let program_id = Pubkey::from_str(program_id_str).unwrap();

    // Attempt to upgrade the program
    config.signers = vec![&keypair, &upgrade_authority];
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: None,
        program_pubkey: Some(program_id),
        buffer_signer_index: None,
        buffer_pubkey: None,
        upgrade_authority_signer_index: 1,
        is_final: false,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: true,
        use_rpc: false,
        skip_feature_verification: true,
    });
    expect_command_failure(
        &config,
        "Can not upgrade a program if it was deployed without the authority signature",
        &format!("Program {program_id} is no longer upgradeable"),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[test_case(true, true; "Feature enabled, skip preflight")]
#[test_case(true, false; "Feature enabled, don't skip preflight")]
#[test_case(false, true; "Feature disabled, skip preflight")]
#[test_case(false, false; "Feature disabled, don't skip preflight")]
async fn test_cli_program_deploy_feature(enable_feature: bool, skip_preflight: bool) {
    agave_logger::setup();

    let mut program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    program_path.push("tests");
    program_path.push("fixtures");
    program_path.push("alt_bn128");
    program_path.set_extension("so");

    let mint_keypair = Keypair::new();
    let mut test_validator_builder = test_validator_genesis(&mint_keypair);

    // Deactivate the enable alt bn128 syscall and try to submit a program with that syscall
    if !enable_feature {
        test_validator_builder.deactivate_features(&[enable_alt_bn128_syscall::id()]);
    }

    let test_validator = test_validator_builder
        .start_async_with_mint_address(&mint_keypair, SocketAddrSpace::Unspecified)
        .await
        .expect("validator start failed");

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    let rpc_client = setup_rpc_client(&mut config);

    let mut file = File::open(program_path.to_str().unwrap()).unwrap();
    let mut program_data = Vec::new();
    file.read_to_end(&mut program_data).unwrap();
    let max_len = program_data.len();
    let minimum_balance_for_programdata = rpc_client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_programdata(
            max_len,
        ))
        .await
        .unwrap();
    let minimum_balance_for_program = rpc_client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_program())
        .await
        .unwrap();
    let upgrade_authority = Keypair::new();

    let keypair = Keypair::new();
    config.command = CliCommand::Airdrop {
        pubkey: None,
        lamports: 100 * minimum_balance_for_programdata + minimum_balance_for_program,
    };
    config.signers = vec![&keypair];
    config.send_transaction_config.skip_preflight = skip_preflight;
    process_command(&config).await.unwrap();

    config.signers = vec![&keypair, &upgrade_authority];
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(program_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: None,
        program_pubkey: None,
        buffer_signer_index: None,
        buffer_pubkey: None,
        upgrade_authority_signer_index: 1,
        is_final: true,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: true,
        use_rpc: false,
        skip_feature_verification: false,
    });
    config.output_format = OutputFormat::JsonCompact;

    if enable_feature {
        let res = process_command(&config).await;
        assert!(res.is_ok());
    } else {
        expect_command_failure(
            &config,
            "Program contains a syscall from a deactivated feature",
            "ELF error: ELF error: Unresolved symbol (sol_alt_bn128_group_op) at instruction #49 \
             (ELF file offset 0x188)",
        )
        .await;

        // If we bypass the verification, there should be no error
        config.command = CliCommand::Program(ProgramCliCommand::Deploy {
            program_location: Some(program_path.to_str().unwrap().to_string()),
            fee_payer_signer_index: 0,
            program_signer_index: None,
            program_pubkey: None,
            buffer_signer_index: None,
            buffer_pubkey: None,
            upgrade_authority_signer_index: 1,
            is_final: true,
            max_len: None,
            skip_fee_check: false,
            compute_unit_price: None,
            max_sign_attempts: 5,
            auto_extend: true,
            use_rpc: false,
            skip_feature_verification: true,
        });

        // When we skip verification, we fail at a later stage
        let response = process_command(&config).await;
        if skip_preflight {
            assert!(response
                .err()
                .unwrap()
                .to_string()
                .contains("Deploying program failed"));
        } else {
            assert!(response
                .err()
                .unwrap()
                .to_string()
                .contains("Deploying program failed: RPC response error -32002:"));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[test_case(true; "Feature enabled")]
#[test_case(false; "Feature disabled")]
async fn test_cli_program_upgrade_with_feature(enable_feature: bool) {
    agave_logger::setup();

    let mut noop_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    noop_path.push("tests");
    noop_path.push("fixtures");
    noop_path.push("noop");
    noop_path.set_extension("so");

    let mut syscall_program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    syscall_program_path.push("tests");
    syscall_program_path.push("fixtures");
    syscall_program_path.push("alt_bn128");
    syscall_program_path.set_extension("so");

    let mint_keypair = Keypair::new();
    let mut test_validator_builder = test_validator_genesis(&mint_keypair);

    // Deactivate the enable alt bn128 syscall and try to submit a program with that syscall
    if !enable_feature {
        test_validator_builder.deactivate_features(&[enable_alt_bn128_syscall::id()]);
    }

    let test_validator = test_validator_builder
        .start_async_with_mint_address(&mint_keypair, SocketAddrSpace::Unspecified)
        .await
        .expect("validator start failed");

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    let rpc_client = setup_rpc_client(&mut config);

    let blockhash = rpc_client.get_latest_blockhash().await.unwrap();

    let mut file = File::open(syscall_program_path.to_str().unwrap()).unwrap();
    let mut large_program_data = Vec::new();
    file.read_to_end(&mut large_program_data).unwrap();
    let max_program_data_len = large_program_data.len();
    let minimum_balance_for_large_buffer = rpc_client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_programdata(
            max_program_data_len,
        ))
        .await
        .unwrap();

    config.send_transaction_config.skip_preflight = false;

    let online_signer = Keypair::new();
    let offline_signer = Keypair::new();
    let buffer_signer = Keypair::new();
    // Typically, keypair for program signer should be different from online signer or
    // offline signer keypairs.
    let program_signer = Keypair::new();

    config.command = CliCommand::Airdrop {
        pubkey: None,
        lamports: 100 * minimum_balance_for_large_buffer, // gotta be enough for this test
    };
    config.signers = vec![&online_signer];
    process_command(&config).await.unwrap();
    config.command = CliCommand::Airdrop {
        pubkey: None,
        lamports: 100 * minimum_balance_for_large_buffer, // gotta be enough for this test
    };
    config.signers = vec![&offline_signer];
    process_command(&config).await.unwrap();

    // Deploy upgradeable program with authority set to offline signer
    config.signers = vec![&online_signer, &offline_signer, &program_signer];
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: Some(2),
        program_pubkey: Some(program_signer.pubkey()),
        buffer_signer_index: None,
        buffer_pubkey: None,
        upgrade_authority_signer_index: 1, // must be offline signer for security reasons
        is_final: false,
        max_len: Some(max_program_data_len), // allows for larger program size with future upgrades
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: true,
        use_rpc: false,
        skip_feature_verification: false,
    });
    config.output_format = OutputFormat::JsonCompact;
    process_command(&config).await.unwrap();

    // Prepare buffer to upgrade deployed program to a larger program
    create_buffer_with_offline_authority(
        &rpc_client,
        &syscall_program_path,
        &mut config,
        &online_signer,
        &offline_signer,
        &buffer_signer,
    )
    .await;

    config.signers = vec![&offline_signer];
    config.command = CliCommand::Program(ProgramCliCommand::Upgrade {
        fee_payer_signer_index: 0,
        program_pubkey: program_signer.pubkey(),
        buffer_pubkey: buffer_signer.pubkey(),
        upgrade_authority_signer_index: 0,
        sign_only: true,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::new(Some(blockhash), true, None),
        skip_feature_verification: false,
    });
    config.output_format = OutputFormat::JsonCompact;
    let sig_response = process_command(&config).await.unwrap();
    let sign_only = parse_sign_only_reply_string(&sig_response);
    let offline_pre_signer = sign_only.presigner_of(&offline_signer.pubkey()).unwrap();
    // Attempt to deploy from buffer using signature over correct message (should succeed)
    config.signers = vec![&offline_pre_signer, &program_signer];

    config.command = CliCommand::Program(ProgramCliCommand::Upgrade {
        fee_payer_signer_index: 0,
        program_pubkey: program_signer.pubkey(),
        buffer_pubkey: buffer_signer.pubkey(),
        upgrade_authority_signer_index: 0,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::new(Some(blockhash), true, None),
        skip_feature_verification: false,
    });
    config.output_format = OutputFormat::JsonCompact;
    if enable_feature {
        let res = process_command(&config).await;
        assert!(res.is_ok());
    } else {
        expect_command_failure(
            &config,
            "Program contains a syscall to a disabled feature",
            format!(
                "Buffer account {} has invalid program data: \"ELF error: ELF error: Unresolved \
                 symbol (sol_alt_bn128_group_op) at instruction #49 (ELF file offset 0x188)\"",
                buffer_signer.pubkey()
            )
            .as_str(),
        )
        .await;

        // If we skip verification, the failure should be at a later stage
        config.command = CliCommand::Program(ProgramCliCommand::Upgrade {
            fee_payer_signer_index: 0,
            program_pubkey: program_signer.pubkey(),
            buffer_pubkey: buffer_signer.pubkey(),
            upgrade_authority_signer_index: 0,
            sign_only: false,
            dump_transaction_message: false,
            blockhash_query: BlockhashQuery::new(Some(blockhash), true, None),
            skip_feature_verification: true,
        });
        config.output_format = OutputFormat::JsonCompact;

        let response = process_command(&config).await;
        assert!(response
            .err()
            .unwrap()
            .to_string()
            .contains("Upgrading program failed: RPC response error -32002"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cli_program_deploy_with_authority() {
    agave_logger::setup();

    let mut noop_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    noop_path.push("tests");
    noop_path.push("fixtures");
    noop_path.push("noop");
    noop_path.set_extension("so");

    let mint_keypair = Keypair::new();
    let test_validator = test_validator_genesis(&mint_keypair)
        .start_async_with_mint_address(&mint_keypair, SocketAddrSpace::Unspecified)
        .await
        .expect("validator start failed");

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    let rpc_client = setup_rpc_client(&mut config);

    let mut file = File::open(noop_path.to_str().unwrap()).unwrap();
    let mut program_data = Vec::new();
    file.read_to_end(&mut program_data).unwrap();
    let max_len = program_data.len();
    let minimum_balance_for_programdata = rpc_client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_programdata(
            max_len,
        ))
        .await
        .unwrap();
    let minimum_balance_for_program = rpc_client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_program())
        .await
        .unwrap();
    let upgrade_authority = Keypair::new();

    let keypair = Keypair::new();
    config.signers = vec![&keypair];
    config.command = CliCommand::Airdrop {
        pubkey: None,
        lamports: 100 * minimum_balance_for_programdata + minimum_balance_for_program,
    };
    process_command(&config).await.unwrap();

    // Deploy the upgradeable program with specified program_id
    let program_keypair = Keypair::new();
    config.signers = vec![&keypair, &upgrade_authority, &program_keypair];
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: Some(2),
        program_pubkey: Some(program_keypair.pubkey()),
        buffer_signer_index: None,
        buffer_pubkey: None,
        upgrade_authority_signer_index: 1,
        is_final: false,
        max_len: Some(max_len),
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: true,
        use_rpc: false,
        skip_feature_verification: true,
    });
    config.output_format = OutputFormat::JsonCompact;
    let response = process_command(&config).await;
    let json: Value = serde_json::from_str(&response.unwrap()).unwrap();
    let program_pubkey_str = json
        .as_object()
        .unwrap()
        .get("programId")
        .unwrap()
        .as_str()
        .unwrap();
    assert_eq!(
        program_keypair.pubkey(),
        Pubkey::from_str(program_pubkey_str).unwrap()
    );
    let program_account = rpc_client
        .get_account(&program_keypair.pubkey())
        .await
        .unwrap();
    assert_eq!(program_account.lamports, minimum_balance_for_program);
    assert_eq!(program_account.owner, bpf_loader_upgradeable::id());
    assert!(program_account.executable);
    let (programdata_pubkey, _) = Pubkey::find_program_address(
        &[program_keypair.pubkey().as_ref()],
        &bpf_loader_upgradeable::id(),
    );
    let programdata_account = rpc_client.get_account(&programdata_pubkey).await.unwrap();
    assert_eq!(
        programdata_account.lamports,
        minimum_balance_for_programdata
    );
    assert_eq!(programdata_account.owner, bpf_loader_upgradeable::id());
    assert!(!programdata_account.executable);
    assert_eq!(
        programdata_account.data[UpgradeableLoaderState::size_of_programdata_metadata()..],
        program_data[..]
    );

    // Deploy the upgradeable program
    config.signers = vec![&keypair, &upgrade_authority];
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: None,
        program_pubkey: None,
        buffer_signer_index: None,
        buffer_pubkey: None,
        upgrade_authority_signer_index: 1,
        is_final: false,
        max_len: Some(max_len),
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: true,
        use_rpc: false,
        skip_feature_verification: true,
    });
    let response = process_command(&config).await;
    let json: Value = serde_json::from_str(&response.unwrap()).unwrap();
    let program_pubkey_str = json
        .as_object()
        .unwrap()
        .get("programId")
        .unwrap()
        .as_str()
        .unwrap();
    let program_pubkey = Pubkey::from_str(program_pubkey_str).unwrap();
    let program_account = rpc_client.get_account(&program_pubkey).await.unwrap();
    assert_eq!(program_account.lamports, minimum_balance_for_program);
    assert_eq!(program_account.owner, bpf_loader_upgradeable::id());
    assert!(program_account.executable);
    let (programdata_pubkey, _) =
        Pubkey::find_program_address(&[program_pubkey.as_ref()], &bpf_loader_upgradeable::id());
    let programdata_account = rpc_client.get_account(&programdata_pubkey).await.unwrap();
    assert_eq!(
        programdata_account.lamports,
        minimum_balance_for_programdata
    );
    assert_eq!(programdata_account.owner, bpf_loader_upgradeable::id());
    assert!(!programdata_account.executable);
    assert_eq!(
        programdata_account.data[UpgradeableLoaderState::size_of_programdata_metadata()..],
        program_data[..]
    );

    // Upgrade the program
    config.signers = vec![&keypair, &upgrade_authority];
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: None,
        program_pubkey: Some(program_pubkey),
        buffer_signer_index: None,
        buffer_pubkey: None,
        upgrade_authority_signer_index: 1,
        is_final: false,
        max_len: Some(max_len),
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: true,
        use_rpc: false,
        skip_feature_verification: true,
    });
    process_command(&config).await.unwrap();
    let program_account = rpc_client.get_account(&program_pubkey).await.unwrap();
    assert_eq!(program_account.lamports, minimum_balance_for_program);
    assert_eq!(program_account.owner, bpf_loader_upgradeable::id());
    assert!(program_account.executable);
    let (programdata_pubkey, _) =
        Pubkey::find_program_address(&[program_pubkey.as_ref()], &bpf_loader_upgradeable::id());
    let programdata_account = rpc_client.get_account(&programdata_pubkey).await.unwrap();
    assert_eq!(
        programdata_account.lamports,
        minimum_balance_for_programdata
    );
    assert_eq!(programdata_account.owner, bpf_loader_upgradeable::id());
    assert!(program_account.executable);
    assert_eq!(
        programdata_account.data[UpgradeableLoaderState::size_of_programdata_metadata()..],
        program_data[..]
    );

    let blockhash = rpc_client.get_latest_blockhash().await.unwrap();
    // Set a new authority sign offline first
    let new_upgrade_authority = Keypair::new();
    config.signers = vec![&keypair, &upgrade_authority, &new_upgrade_authority];
    config.command = CliCommand::Program(ProgramCliCommand::SetUpgradeAuthorityChecked {
        program_pubkey,
        upgrade_authority_index: 1,
        new_upgrade_authority_index: 2,
        sign_only: true,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::new(Some(blockhash), true, None),
    });
    let sig_response = process_command(&config).await.unwrap();
    let sign_only = parse_sign_only_reply_string(&sig_response);
    let offline_pre_signer = sign_only
        .presigner_of(&new_upgrade_authority.pubkey())
        .unwrap();

    config.signers = vec![&keypair, &upgrade_authority, &offline_pre_signer];
    config.command = CliCommand::Program(ProgramCliCommand::SetUpgradeAuthorityChecked {
        program_pubkey,
        upgrade_authority_index: 1,
        new_upgrade_authority_index: 2,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::new(Some(blockhash), false, None),
    });
    let response = process_command(&config).await;
    let json: Value = serde_json::from_str(&response.unwrap()).unwrap();
    let new_upgrade_authority_str = json
        .as_object()
        .unwrap()
        .get("authority")
        .unwrap()
        .as_str()
        .unwrap();
    assert_eq!(
        Pubkey::from_str(new_upgrade_authority_str).unwrap(),
        new_upgrade_authority.pubkey()
    );

    // Upgrade with new authority
    config.signers = vec![&keypair, &new_upgrade_authority];
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: None,
        program_pubkey: Some(program_pubkey),
        buffer_signer_index: None,
        buffer_pubkey: None,
        upgrade_authority_signer_index: 1,
        is_final: false,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: true,
        use_rpc: false,
        skip_feature_verification: true,
    });
    process_command(&config).await.unwrap();
    let program_account = rpc_client.get_account(&program_pubkey).await.unwrap();
    assert_eq!(program_account.lamports, minimum_balance_for_program);
    assert_eq!(program_account.owner, bpf_loader_upgradeable::id());
    assert!(program_account.executable);
    let (programdata_pubkey, _) =
        Pubkey::find_program_address(&[program_pubkey.as_ref()], &bpf_loader_upgradeable::id());
    let programdata_account = rpc_client.get_account(&programdata_pubkey).await.unwrap();
    assert_eq!(
        programdata_account.lamports,
        minimum_balance_for_programdata
    );
    assert_eq!(programdata_account.owner, bpf_loader_upgradeable::id());
    assert!(program_account.executable);
    assert_eq!(
        programdata_account.data[UpgradeableLoaderState::size_of_programdata_metadata()..],
        program_data[..]
    );

    // Get upgrade authority
    config.signers = vec![&keypair];
    config.command = CliCommand::Program(ProgramCliCommand::Show {
        account_pubkey: Some(program_pubkey),
        authority_pubkey: keypair.pubkey(),
        get_programs: false,
        get_buffers: false,
        all: false,
        use_lamports_unit: false,
    });
    let response = process_command(&config).await;
    let json: Value = serde_json::from_str(&response.unwrap()).unwrap();
    let authority_pubkey_str = json
        .as_object()
        .unwrap()
        .get("authority")
        .unwrap()
        .as_str()
        .unwrap();
    assert_eq!(
        new_upgrade_authority.pubkey(),
        Pubkey::from_str(authority_pubkey_str).unwrap()
    );

    // Set no authority
    config.signers = vec![&keypair, &new_upgrade_authority];
    config.command = CliCommand::Program(ProgramCliCommand::SetUpgradeAuthority {
        program_pubkey,
        upgrade_authority_index: Some(1),
        new_upgrade_authority: None,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::default(),
    });
    let response = process_command(&config).await;
    let json: Value = serde_json::from_str(&response.unwrap()).unwrap();
    let new_upgrade_authority_str = json
        .as_object()
        .unwrap()
        .get("authority")
        .unwrap()
        .as_str()
        .unwrap();
    assert_eq!(new_upgrade_authority_str, "none");

    // Upgrade with no authority
    config.signers = vec![&keypair, &new_upgrade_authority];
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: None,
        program_pubkey: Some(program_pubkey),
        buffer_signer_index: None,
        buffer_pubkey: None,
        upgrade_authority_signer_index: 1,
        is_final: false,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: true,
        use_rpc: false,
        skip_feature_verification: true,
    });
    expect_command_failure(
        &config,
        "Upgrade without an authority is not allowed",
        &format!("Program {program_pubkey} is no longer upgradeable"),
    )
    .await;

    // deploy with finality
    config.signers = vec![&keypair, &new_upgrade_authority];
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: None,
        program_pubkey: None,
        buffer_signer_index: None,
        buffer_pubkey: None,
        upgrade_authority_signer_index: 1,
        is_final: true,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: true,
        use_rpc: false,
        skip_feature_verification: true,
    });
    let response = process_command(&config).await;
    let json: Value = serde_json::from_str(&response.unwrap()).unwrap();
    let program_pubkey_str = json
        .as_object()
        .unwrap()
        .get("programId")
        .unwrap()
        .as_str()
        .unwrap();
    let program_pubkey = Pubkey::from_str(program_pubkey_str).unwrap();
    let (programdata_pubkey, _) =
        Pubkey::find_program_address(&[program_pubkey.as_ref()], &bpf_loader_upgradeable::id());
    let programdata_account = rpc_client.get_account(&programdata_pubkey).await.unwrap();
    if let UpgradeableLoaderState::ProgramData {
        slot: _,
        upgrade_authority_address,
    } = programdata_account.state().unwrap()
    {
        assert_eq!(upgrade_authority_address, None);
    } else {
        panic!("not a ProgramData account");
    }

    // Get buffer authority
    config.signers = vec![&keypair];
    config.command = CliCommand::Program(ProgramCliCommand::Show {
        account_pubkey: Some(program_pubkey),
        authority_pubkey: keypair.pubkey(),
        get_programs: false,
        get_buffers: false,
        all: false,
        use_lamports_unit: false,
    });
    let response = process_command(&config).await;
    let json: Value = serde_json::from_str(&response.unwrap()).unwrap();
    let authority_pubkey_str = json
        .as_object()
        .unwrap()
        .get("authority")
        .unwrap()
        .as_str()
        .unwrap();
    assert_eq!("none", authority_pubkey_str);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[test_case(true; "Skip preflight")]
#[test_case(false; "Dont skip preflight")]
async fn test_cli_program_upgrade_auto_extend(skip_preflight: bool) {
    agave_logger::setup();

    let mut noop_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    noop_path.push("tests");
    noop_path.push("fixtures");
    noop_path.push("noop");
    noop_path.set_extension("so");

    let mut noop_large_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    noop_large_path.push("tests");
    noop_large_path.push("fixtures");
    noop_large_path.push("noop_large");
    noop_large_path.set_extension("so");

    let mint_keypair = Keypair::new();
    let test_validator = test_validator_genesis(&mint_keypair)
        .start_async_with_mint_address(&mint_keypair, SocketAddrSpace::Unspecified)
        .await
        .expect("validator start failed");

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    let rpc_client = setup_rpc_client(&mut config);

    let mut file = File::open(noop_path.to_str().unwrap()).unwrap();
    let mut program_data = Vec::new();
    file.read_to_end(&mut program_data).unwrap();

    let mut file = File::open(noop_large_path.to_str().unwrap()).unwrap();
    let mut program_data_large = Vec::new();
    file.read_to_end(&mut program_data_large).unwrap();

    // Use the larger program to calculate rent.
    let max_len = program_data_large.len();
    let minimum_balance_for_programdata = rpc_client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_programdata(
            max_len,
        ))
        .await
        .unwrap();
    let minimum_balance_for_program = rpc_client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_program())
        .await
        .unwrap();
    let upgrade_authority = Keypair::new();

    let keypair = Keypair::new();
    config.signers = vec![&keypair];
    config.command = CliCommand::Airdrop {
        pubkey: None,
        lamports: 100 * minimum_balance_for_programdata + minimum_balance_for_program,
    };
    process_command(&config).await.unwrap();

    // Deploy the first, smaller program.
    let program_keypair = Keypair::new();
    config.signers = vec![&keypair, &upgrade_authority, &program_keypair];
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: Some(2),
        program_pubkey: Some(program_keypair.pubkey()),
        buffer_signer_index: None,
        buffer_pubkey: None,
        upgrade_authority_signer_index: 1,
        is_final: false,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: true,
        use_rpc: false,
        skip_feature_verification: true,
    });
    config.output_format = OutputFormat::JsonCompact;
    config.send_transaction_config.skip_preflight = skip_preflight;
    process_command(&config).await.unwrap();

    // Attempt to upgrade the program with a larger program, but with the
    // --no-auto-extend flag.
    config.signers = vec![&keypair, &upgrade_authority];
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_large_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: None,
        program_pubkey: Some(program_keypair.pubkey()),
        buffer_signer_index: None,
        buffer_pubkey: None,
        upgrade_authority_signer_index: 1,
        is_final: true,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: false, // --no-auto-extend flag is present
        use_rpc: false,
        skip_feature_verification: true,
    });
    if skip_preflight {
        expect_command_failure(
            &config,
            "Cannot upgrade a program when ELF does not fit into the allocated data account",
            "Deploying program failed: Error processing Instruction 0: account data too small for \
             instruction",
        )
        .await;
    } else {
        #[rustfmt::skip]
        let expected_error =
            "Deploying program failed: \
             RPC response error -32002: \
             Transaction simulation failed: \
             Error processing Instruction 0: \
             account data too small for instruction; 3 log messages:\n  \
             Program BPFLoaderUpgradeab1e11111111111111111111111 invoke [1]\n  \
             ProgramData account not large enough\n  \
             Program BPFLoaderUpgradeab1e11111111111111111111111 failed: account data too small \
             for instruction\n";
        expect_command_failure(
            &config,
            "Can not upgrade a program when ELF does not fit into the allocated data account",
            expected_error,
        )
        .await;
    }

    // Attempt to upgrade the program with a larger program, this time without
    // the --no-auto-extend flag. This should automatically extend the program data.
    config.signers = vec![&keypair, &upgrade_authority];
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_large_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: None,
        program_pubkey: Some(program_keypair.pubkey()),
        buffer_signer_index: None,
        buffer_pubkey: None,
        upgrade_authority_signer_index: 1,
        is_final: true,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: true, // --no-auto-extend flag is absent
        use_rpc: false,
        skip_feature_verification: true,
    });
    let response = process_command(&config).await;
    let json: Value = serde_json::from_str(&response.unwrap()).unwrap();
    let program_pubkey_str = json
        .as_object()
        .unwrap()
        .get("programId")
        .unwrap()
        .as_str()
        .unwrap();
    let program_pubkey = Pubkey::from_str(program_pubkey_str).unwrap();
    let (programdata_pubkey, _) =
        Pubkey::find_program_address(&[program_pubkey.as_ref()], &bpf_loader_upgradeable::id());
    let programdata_account = rpc_client.get_account(&programdata_pubkey).await.unwrap();
    if let UpgradeableLoaderState::ProgramData {
        slot: _,
        upgrade_authority_address,
    } = programdata_account.state().unwrap()
    {
        assert_eq!(upgrade_authority_address, None);
    } else {
        panic!("not a ProgramData account");
    }
    assert_eq!(
        programdata_account.data().len(),
        UpgradeableLoaderState::size_of_programdata(program_data_large.len()),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cli_program_close_program() {
    agave_logger::setup();

    let mut noop_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    noop_path.push("tests");
    noop_path.push("fixtures");
    noop_path.push("noop");
    noop_path.set_extension("so");

    let mint_keypair = Keypair::new();
    let test_validator = test_validator_genesis(&mint_keypair)
        .start_async_with_mint_address(&mint_keypair, SocketAddrSpace::Unspecified)
        .await
        .expect("validator start failed");

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    let rpc_client = setup_rpc_client(&mut config);

    let mut file = File::open(noop_path.to_str().unwrap()).unwrap();
    let mut program_data = Vec::new();
    file.read_to_end(&mut program_data).unwrap();
    let max_len = program_data.len();
    let minimum_balance_for_programdata = rpc_client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_programdata(
            max_len,
        ))
        .await
        .unwrap();
    let minimum_balance_for_program = rpc_client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_program())
        .await
        .unwrap();
    let upgrade_authority = Keypair::new();

    let keypair = Keypair::new();
    config.signers = vec![&keypair];
    config.command = CliCommand::Airdrop {
        pubkey: None,
        lamports: 100 * minimum_balance_for_programdata + minimum_balance_for_program,
    };
    process_command(&config).await.unwrap();

    // Deploy the upgradeable program
    let program_keypair = Keypair::new();
    config.signers = vec![&keypair, &upgrade_authority, &program_keypair];
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: Some(2),
        program_pubkey: Some(program_keypair.pubkey()),
        buffer_signer_index: None,
        buffer_pubkey: None,
        upgrade_authority_signer_index: 1,
        is_final: false,
        max_len: Some(max_len),
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: true,
        use_rpc: false,
        skip_feature_verification: true,
    });
    config.output_format = OutputFormat::JsonCompact;
    process_command(&config).await.unwrap();

    let (programdata_pubkey, _) = Pubkey::find_program_address(
        &[program_keypair.pubkey().as_ref()],
        &bpf_loader_upgradeable::id(),
    );

    // Wait one slot to avoid "Program was deployed in this block already" error
    wait_n_slots(&rpc_client, 1).await;

    // Close program
    let close_account = rpc_client.get_account(&programdata_pubkey).await.unwrap();
    let programdata_lamports = close_account.lamports;
    let recipient_pubkey = Pubkey::new_unique();
    config.signers = vec![&keypair, &upgrade_authority];

    // Close without --bypass-warning flag
    config.command = CliCommand::Program(ProgramCliCommand::Close {
        account_pubkey: Some(program_keypair.pubkey()),
        recipient_pubkey,
        authority_index: 1,
        use_lamports_unit: false,
        bypass_warning: false,
    });
    expect_command_failure(
        &config,
        "CLI requires the --bypass-warning flag in order to close a program",
        CLOSE_PROGRAM_WARNING,
    )
    .await;

    // Close with --bypass-warning flag
    config.command = CliCommand::Program(ProgramCliCommand::Close {
        account_pubkey: Some(program_keypair.pubkey()),
        recipient_pubkey,
        authority_index: 1,
        use_lamports_unit: false,
        bypass_warning: true,
    });
    process_command(&config).await.unwrap();

    expect_account_absent(
        &rpc_client,
        programdata_pubkey,
        "Program data account is deleted when the program is closed",
    )
    .await;
    let recipient_account = rpc_client.get_account(&recipient_pubkey).await.unwrap();
    assert_eq!(programdata_lamports, recipient_account.lamports);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cli_program_extend_program() {
    agave_logger::setup();

    let mut noop_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    noop_path.push("tests");
    noop_path.push("fixtures");
    noop_path.push("noop");
    noop_path.set_extension("so");

    let mut noop_large_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    noop_large_path.push("tests");
    noop_large_path.push("fixtures");
    noop_large_path.push("noop_large");
    noop_large_path.set_extension("so");

    let mint_keypair = Keypair::new();
    let test_validator = test_validator_genesis(&mint_keypair)
        .start_async_with_mint_address(&mint_keypair, SocketAddrSpace::Unspecified)
        .await
        .expect("validator start failed");

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    let rpc_client = setup_rpc_client(&mut config);

    let mut file = File::open(noop_path.to_str().unwrap()).unwrap();
    let mut program_data = Vec::new();
    file.read_to_end(&mut program_data).unwrap();
    let max_len = program_data.len();
    let minimum_balance_for_programdata = rpc_client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_programdata(
            max_len,
        ))
        .await
        .unwrap();
    let minimum_balance_for_program = rpc_client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_program())
        .await
        .unwrap();
    let upgrade_authority = Keypair::new();

    let keypair = Keypair::new();
    config.signers = vec![&keypair];
    config.command = CliCommand::Airdrop {
        pubkey: None,
        lamports: 100 * minimum_balance_for_programdata + minimum_balance_for_program,
    };
    config.send_transaction_config = RpcSendTransactionConfig {
        skip_preflight: false,
        preflight_commitment: Some(CommitmentConfig::processed().commitment),
        ..RpcSendTransactionConfig::default()
    };
    process_command(&config).await.unwrap();

    // Deploy an upgradeable program
    let program_keypair = Keypair::new();
    config.signers = vec![&keypair, &upgrade_authority, &program_keypair];
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: Some(2),
        program_pubkey: Some(program_keypair.pubkey()),
        buffer_signer_index: None,
        buffer_pubkey: None,
        upgrade_authority_signer_index: 1,
        is_final: false,
        max_len: None, // Use None to check that it defaults to the max length
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: false,
        use_rpc: false,
        skip_feature_verification: true,
    });
    config.output_format = OutputFormat::JsonCompact;
    process_command(&config).await.unwrap();

    let (programdata_pubkey, _) = Pubkey::find_program_address(
        &[program_keypair.pubkey().as_ref()],
        &bpf_loader_upgradeable::id(),
    );

    let programdata_account = rpc_client.get_account(&programdata_pubkey).await.unwrap();
    let expected_len = UpgradeableLoaderState::size_of_programdata(max_len);
    assert_eq!(expected_len, programdata_account.data.len());

    // Wait one slot to avoid "Program was deployed in this block already" error
    wait_n_slots(&rpc_client, 1).await;

    // Extend program for larger program, minus 1 required byte
    let mut file = File::open(noop_large_path.to_str().unwrap()).unwrap();
    let mut new_program_data = Vec::new();
    file.read_to_end(&mut new_program_data).unwrap();
    let new_max_len = new_program_data.len();
    let additional_bytes = (new_max_len - max_len) as u32;
    config.signers = vec![&keypair, &upgrade_authority];
    config.command = CliCommand::Program(ProgramCliCommand::ExtendProgramChecked {
        program_pubkey: program_keypair.pubkey(),
        authority_signer_index: 1,
        additional_bytes: additional_bytes - 1,
    });
    process_command(&config).await.unwrap();

    let programdata_account = rpc_client.get_account(&programdata_pubkey).await.unwrap();
    let expected_len = UpgradeableLoaderState::size_of_programdata(new_max_len - 1);
    assert_eq!(expected_len, programdata_account.data.len());

    // Larger program deploy fails because missing 1 byte
    config.signers = vec![&keypair, &upgrade_authority];
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_large_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: None,
        program_pubkey: Some(program_keypair.pubkey()),
        buffer_signer_index: None,
        buffer_pubkey: None,
        upgrade_authority_signer_index: 1,
        is_final: false,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: false,
        use_rpc: false,
        skip_feature_verification: true,
    });

    #[rustfmt::skip]
    let expected_error =
        "Deploying program failed: \
         RPC response error -32002: \
         Transaction simulation failed: \
         Error processing Instruction 0: \
         account data too small for instruction; 3 log messages:\n  \
         Program BPFLoaderUpgradeab1e11111111111111111111111 invoke [1]\n  \
         ProgramData account not large enough\n  \
         Program BPFLoaderUpgradeab1e11111111111111111111111 failed: account data too small for \
         instruction\n";
    expect_command_failure(
        &config,
        "Program upgrade must fail, as the buffer is 1 byte too short",
        expected_error,
    )
    .await;

    // Wait one slot to avoid "Program was deployed in this block already" error
    wait_n_slots(&rpc_client, 1).await;

    // Extend 1 last byte
    config.signers = vec![&keypair, &upgrade_authority];
    config.command = CliCommand::Program(ProgramCliCommand::ExtendProgramChecked {
        program_pubkey: program_keypair.pubkey(),
        authority_signer_index: 1,
        additional_bytes: 1,
    });
    process_command(&config).await.unwrap();

    let programdata_account = rpc_client.get_account(&programdata_pubkey).await.unwrap();
    let expected_len = UpgradeableLoaderState::size_of_programdata(new_max_len);
    assert_eq!(expected_len, programdata_account.data.len());

    // Larger program deploy finally succeeds
    config.signers = vec![&keypair, &upgrade_authority];
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_large_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: None,
        program_pubkey: Some(program_keypair.pubkey()),
        buffer_signer_index: None,
        buffer_pubkey: None,
        upgrade_authority_signer_index: 1,
        is_final: false,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: false,
        use_rpc: false,
        skip_feature_verification: true,
    });
    process_command(&config).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cli_program_migrate_program() {
    agave_logger::setup();

    let mut noop_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    noop_path.push("tests");
    noop_path.push("fixtures");
    noop_path.push("noop");
    noop_path.set_extension("so");

    let mint_keypair = Keypair::new();
    let test_validator = test_validator_genesis(&mint_keypair)
        .start_async_with_mint_address(&mint_keypair, SocketAddrSpace::Unspecified)
        .await
        .expect("validator start failed");

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    let rpc_client = setup_rpc_client(&mut config);

    let mut file = File::open(noop_path.to_str().unwrap()).unwrap();
    let mut program_data = Vec::new();
    file.read_to_end(&mut program_data).unwrap();
    let max_len = program_data.len();
    let minimum_balance_for_programdata = rpc_client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_programdata(
            max_len,
        ))
        .await
        .unwrap();
    let minimum_balance_for_program = rpc_client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_program())
        .await
        .unwrap();
    let upgrade_authority = Keypair::new();

    let keypair = Keypair::new();
    config.signers = vec![&keypair];
    config.command = CliCommand::Airdrop {
        pubkey: None,
        lamports: 100 * minimum_balance_for_programdata + minimum_balance_for_program,
    };
    process_command(&config).await.unwrap();

    // Deploy the upgradeable program
    let program_keypair = Keypair::new();
    config.signers = vec![&keypair, &upgrade_authority, &program_keypair];
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: Some(2),
        program_pubkey: Some(program_keypair.pubkey()),
        buffer_signer_index: None,
        buffer_pubkey: None,
        upgrade_authority_signer_index: 1,
        is_final: false,
        max_len: Some(max_len),
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: true,
        use_rpc: false,
        skip_feature_verification: true,
    });
    config.output_format = OutputFormat::JsonCompact;
    process_command(&config).await.unwrap();

    // Wait one slot to avoid "Program was deployed in this block already" error
    wait_n_slots(&rpc_client, 1).await;

    // Migrate program
    config.signers = vec![&keypair, &upgrade_authority];
    config.command = CliCommand::Program(ProgramCliCommand::MigrateProgram {
        program_pubkey: program_keypair.pubkey(),
        authority_signer_index: 1,
        compute_unit_price: Some(1),
    });
    process_command(&config).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cli_program_write_buffer() {
    agave_logger::setup();

    let mut noop_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    noop_path.push("tests");
    noop_path.push("fixtures");
    noop_path.push("noop");
    noop_path.set_extension("so");

    let mut noop_large_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    noop_large_path.push("tests");
    noop_large_path.push("fixtures");
    noop_large_path.push("noop_large");
    noop_large_path.set_extension("so");

    let mint_keypair = Keypair::new();
    let test_validator = test_validator_genesis(&mint_keypair)
        .start_async_with_mint_address(&mint_keypair, SocketAddrSpace::Unspecified)
        .await
        .expect("validator start failed");

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    let rpc_client = setup_rpc_client(&mut config);

    let mut file = File::open(noop_path.to_str().unwrap()).unwrap();
    let mut program_data = Vec::new();
    file.read_to_end(&mut program_data).unwrap();
    let max_len = program_data.len();
    let minimum_balance_for_buffer = rpc_client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_programdata(
            max_len,
        ))
        .await
        .unwrap();
    let minimum_balance_for_buffer_default = rpc_client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_programdata(
            max_len,
        ))
        .await
        .unwrap();

    let keypair = Keypair::new();
    config.signers = vec![&keypair];
    config.command = CliCommand::Airdrop {
        pubkey: None,
        lamports: 100 * minimum_balance_for_buffer,
    };
    process_command(&config).await.unwrap();

    // Write a buffer with default params
    config.signers = vec![&keypair];
    config.command = CliCommand::Program(ProgramCliCommand::WriteBuffer {
        program_location: noop_path.to_str().unwrap().to_string(),
        fee_payer_signer_index: 0,
        buffer_signer_index: None,
        buffer_pubkey: None,
        buffer_authority_signer_index: 0,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        use_rpc: false,
        skip_feature_verification: true,
    });
    config.output_format = OutputFormat::JsonCompact;
    let response = process_command(&config).await;
    let json: Value = serde_json::from_str(&response.unwrap()).unwrap();
    let buffer_pubkey_str = json
        .as_object()
        .unwrap()
        .get("buffer")
        .unwrap()
        .as_str()
        .unwrap();
    let new_buffer_pubkey = Pubkey::from_str(buffer_pubkey_str).unwrap();
    let buffer_account = rpc_client.get_account(&new_buffer_pubkey).await.unwrap();
    assert_eq!(buffer_account.lamports, minimum_balance_for_buffer_default);
    assert_eq!(buffer_account.owner, bpf_loader_upgradeable::id());
    if let UpgradeableLoaderState::Buffer { authority_address } = buffer_account.state().unwrap() {
        assert_eq!(authority_address, Some(keypair.pubkey()));
    } else {
        panic!("not a buffer account");
    }
    assert_eq!(
        buffer_account.data[UpgradeableLoaderState::size_of_buffer_metadata()..],
        program_data[..]
    );

    // Specify buffer keypair and max_len
    let buffer_keypair = Keypair::new();
    config.signers = vec![&keypair, &buffer_keypair];
    config.command = CliCommand::Program(ProgramCliCommand::WriteBuffer {
        program_location: noop_path.to_str().unwrap().to_string(),
        fee_payer_signer_index: 0,
        buffer_signer_index: Some(1),
        buffer_pubkey: Some(buffer_keypair.pubkey()),
        buffer_authority_signer_index: 0,
        max_len: Some(max_len),
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        use_rpc: false,
        skip_feature_verification: true,
    });
    let response = process_command(&config).await;
    let json: Value = serde_json::from_str(&response.unwrap()).unwrap();
    let buffer_pubkey_str = json
        .as_object()
        .unwrap()
        .get("buffer")
        .unwrap()
        .as_str()
        .unwrap();
    assert_eq!(
        buffer_keypair.pubkey(),
        Pubkey::from_str(buffer_pubkey_str).unwrap()
    );
    let buffer_account = rpc_client
        .get_account(&buffer_keypair.pubkey())
        .await
        .unwrap();
    assert_eq!(buffer_account.lamports, minimum_balance_for_buffer);
    assert_eq!(buffer_account.owner, bpf_loader_upgradeable::id());
    if let UpgradeableLoaderState::Buffer { authority_address } = buffer_account.state().unwrap() {
        assert_eq!(authority_address, Some(keypair.pubkey()));
    } else {
        panic!("not a buffer account");
    }
    assert_eq!(
        buffer_account.data[UpgradeableLoaderState::size_of_buffer_metadata()..],
        program_data[..]
    );

    // Get buffer authority
    config.signers = vec![];
    config.command = CliCommand::Program(ProgramCliCommand::Show {
        account_pubkey: Some(buffer_keypair.pubkey()),
        authority_pubkey: keypair.pubkey(),
        get_programs: false,
        get_buffers: false,
        all: false,
        use_lamports_unit: false,
    });
    let response = process_command(&config).await;
    let json: Value = serde_json::from_str(&response.unwrap()).unwrap();
    let authority_pubkey_str = json
        .as_object()
        .unwrap()
        .get("authority")
        .unwrap()
        .as_str()
        .unwrap();
    assert_eq!(
        keypair.pubkey(),
        Pubkey::from_str(authority_pubkey_str).unwrap()
    );

    // Specify buffer authority
    let buffer_keypair = Keypair::new();
    let authority_keypair = Keypair::new();
    config.signers = vec![&keypair, &buffer_keypair, &authority_keypair];
    config.command = CliCommand::Program(ProgramCliCommand::WriteBuffer {
        program_location: noop_path.to_str().unwrap().to_string(),
        fee_payer_signer_index: 0,
        buffer_signer_index: Some(1),
        buffer_pubkey: Some(buffer_keypair.pubkey()),
        buffer_authority_signer_index: 2,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        use_rpc: false,
        skip_feature_verification: true,
    });
    let response = process_command(&config).await;
    let json: Value = serde_json::from_str(&response.unwrap()).unwrap();
    let buffer_pubkey_str = json
        .as_object()
        .unwrap()
        .get("buffer")
        .unwrap()
        .as_str()
        .unwrap();
    assert_eq!(
        buffer_keypair.pubkey(),
        Pubkey::from_str(buffer_pubkey_str).unwrap()
    );
    let buffer_account = rpc_client
        .get_account(&buffer_keypair.pubkey())
        .await
        .unwrap();
    assert_eq!(buffer_account.lamports, minimum_balance_for_buffer_default);
    assert_eq!(buffer_account.owner, bpf_loader_upgradeable::id());
    if let UpgradeableLoaderState::Buffer { authority_address } = buffer_account.state().unwrap() {
        assert_eq!(authority_address, Some(authority_keypair.pubkey()));
    } else {
        panic!("not a buffer account");
    }
    assert_eq!(
        buffer_account.data[UpgradeableLoaderState::size_of_buffer_metadata()..],
        program_data[..]
    );

    // Specify authority only
    let buffer_keypair = Keypair::new();
    let authority_keypair = Keypair::new();
    config.signers = vec![&keypair, &buffer_keypair, &authority_keypair];
    config.command = CliCommand::Program(ProgramCliCommand::WriteBuffer {
        program_location: noop_path.to_str().unwrap().to_string(),
        fee_payer_signer_index: 0,
        buffer_signer_index: None,
        buffer_pubkey: None,
        buffer_authority_signer_index: 2,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        use_rpc: false,
        skip_feature_verification: true,
    });
    let response = process_command(&config).await;
    let json: Value = serde_json::from_str(&response.unwrap()).unwrap();
    let buffer_pubkey_str = json
        .as_object()
        .unwrap()
        .get("buffer")
        .unwrap()
        .as_str()
        .unwrap();
    let buffer_pubkey = Pubkey::from_str(buffer_pubkey_str).unwrap();
    let buffer_account = rpc_client.get_account(&buffer_pubkey).await.unwrap();
    assert_eq!(buffer_account.lamports, minimum_balance_for_buffer_default);
    assert_eq!(buffer_account.owner, bpf_loader_upgradeable::id());
    if let UpgradeableLoaderState::Buffer { authority_address } = buffer_account.state().unwrap() {
        assert_eq!(authority_address, Some(authority_keypair.pubkey()));
    } else {
        panic!("not a buffer account");
    }
    assert_eq!(
        buffer_account.data[UpgradeableLoaderState::size_of_buffer_metadata()..],
        program_data[..]
    );

    // Get buffer authority
    config.signers = vec![];
    config.command = CliCommand::Program(ProgramCliCommand::Show {
        account_pubkey: Some(buffer_pubkey),
        authority_pubkey: keypair.pubkey(),
        get_programs: false,
        get_buffers: false,
        all: false,
        use_lamports_unit: false,
    });
    let response = process_command(&config).await;
    let json: Value = serde_json::from_str(&response.unwrap()).unwrap();
    let authority_pubkey_str = json
        .as_object()
        .unwrap()
        .get("authority")
        .unwrap()
        .as_str()
        .unwrap();
    assert_eq!(
        authority_keypair.pubkey(),
        Pubkey::from_str(authority_pubkey_str).unwrap()
    );

    // Close buffer
    let close_account = rpc_client.get_account(&buffer_pubkey).await.unwrap();
    assert_eq!(minimum_balance_for_buffer, close_account.lamports);
    let recipient_pubkey = Pubkey::new_unique();
    config.signers = vec![&keypair, &authority_keypair];
    config.command = CliCommand::Program(ProgramCliCommand::Close {
        account_pubkey: Some(buffer_pubkey),
        recipient_pubkey,
        authority_index: 1,
        use_lamports_unit: false,
        bypass_warning: false,
    });
    process_command(&config).await.unwrap();
    expect_account_absent(
        &rpc_client,
        buffer_pubkey,
        "Buffer account is deleted when the buffer is closed",
    )
    .await;
    let recipient_account = rpc_client.get_account(&recipient_pubkey).await.unwrap();
    assert_eq!(minimum_balance_for_buffer, recipient_account.lamports);

    // Write a buffer with default params
    config.signers = vec![&keypair];
    config.command = CliCommand::Program(ProgramCliCommand::WriteBuffer {
        program_location: noop_path.to_str().unwrap().to_string(),
        fee_payer_signer_index: 0,
        buffer_signer_index: None,
        buffer_pubkey: None,
        buffer_authority_signer_index: 0,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        use_rpc: false,
        skip_feature_verification: true,
    });
    config.output_format = OutputFormat::JsonCompact;
    let response = process_command(&config).await;
    let json: Value = serde_json::from_str(&response.unwrap()).unwrap();
    let buffer_pubkey_str = json
        .as_object()
        .unwrap()
        .get("buffer")
        .unwrap()
        .as_str()
        .unwrap();
    let new_buffer_pubkey = Pubkey::from_str(buffer_pubkey_str).unwrap();

    // Close buffers and deposit default keypair
    let pre_lamports = rpc_client
        .get_account(&keypair.pubkey())
        .await
        .unwrap()
        .lamports;
    config.signers = vec![&keypair];
    config.command = CliCommand::Program(ProgramCliCommand::Close {
        account_pubkey: Some(new_buffer_pubkey),
        recipient_pubkey: keypair.pubkey(),
        authority_index: 0,
        use_lamports_unit: false,
        bypass_warning: false,
    });
    process_command(&config).await.unwrap();
    expect_account_absent(
        &rpc_client,
        new_buffer_pubkey,
        "Buffer account is deleted when the buffer is closed",
    )
    .await;
    let recipient_account = rpc_client.get_account(&keypair.pubkey()).await.unwrap();
    assert_eq!(
        pre_lamports + minimum_balance_for_buffer,
        recipient_account.lamports
    );

    // write small buffer then attempt to deploy larger program
    let buffer_keypair = Keypair::new();
    config.signers = vec![&keypair, &buffer_keypair];
    config.command = CliCommand::Program(ProgramCliCommand::WriteBuffer {
        program_location: noop_path.to_str().unwrap().to_string(),
        fee_payer_signer_index: 0,
        buffer_signer_index: Some(1),
        buffer_pubkey: Some(buffer_keypair.pubkey()),
        buffer_authority_signer_index: 0,
        max_len: None, //Some(max_len),
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        use_rpc: false,
        skip_feature_verification: true,
    });
    process_command(&config).await.unwrap();
    config.signers = vec![&keypair, &buffer_keypair];
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_large_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: None,
        program_pubkey: None,
        buffer_signer_index: Some(1),
        buffer_pubkey: Some(buffer_keypair.pubkey()),
        upgrade_authority_signer_index: 0,
        is_final: true,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: true,
        use_rpc: false,
        skip_feature_verification: true,
    });
    config.output_format = OutputFormat::JsonCompact;
    let buffer_account_len = {
        let mut file = File::open(noop_path.to_str().unwrap()).unwrap();
        let program_data_len = file.seek(SeekFrom::End(0)).unwrap() as usize;
        UpgradeableLoaderState::size_of_buffer_metadata() + program_data_len
    };
    let min_buffer_account_len = {
        let mut file = File::open(noop_large_path.to_str().unwrap()).unwrap();
        let large_program_data_len = file.seek(SeekFrom::End(0)).unwrap() as usize;
        UpgradeableLoaderState::size_of_buffer_metadata() + large_program_data_len
    };
    expect_command_failure(
        &config,
        "It should not be possible to deploy a program into an account that is too small",
        &format!(
            "Buffer account data size ({buffer_account_len}) is smaller than the minimum size \
             ({min_buffer_account_len})"
        ),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[test_case(true; "Feature enabled")]
#[test_case(false; "Feature disabled")]
async fn test_cli_program_write_buffer_feature(enable_feature: bool) {
    agave_logger::setup();

    let mut program_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    program_path.push("tests");
    program_path.push("fixtures");
    program_path.push("alt_bn128");
    program_path.set_extension("so");

    let mint_keypair = Keypair::new();
    let mut test_validator_builder = test_validator_genesis(&mint_keypair);

    // Deactivate the enable alt bn128 syscall and try to submit a program with that syscall
    if !enable_feature {
        test_validator_builder.deactivate_features(&[enable_alt_bn128_syscall::id()]);
    }

    let test_validator = test_validator_builder
        .start_async_with_mint_address(&mint_keypair, SocketAddrSpace::Unspecified)
        .await
        .expect("validator start failed");

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    let rpc_client = setup_rpc_client(&mut config);

    let mut file = File::open(program_path.to_str().unwrap()).unwrap();
    let mut program_data = Vec::new();
    file.read_to_end(&mut program_data).unwrap();
    let max_len = program_data.len();
    let minimum_balance_for_buffer = rpc_client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_programdata(
            max_len,
        ))
        .await
        .unwrap();

    let keypair = Keypair::new();
    config.signers = vec![&keypair];
    config.command = CliCommand::Airdrop {
        pubkey: None,
        lamports: 100 * minimum_balance_for_buffer,
    };
    process_command(&config).await.unwrap();

    // Write a buffer with default params
    config.signers = vec![&keypair];
    config.command = CliCommand::Program(ProgramCliCommand::WriteBuffer {
        program_location: program_path.to_str().unwrap().to_string(),
        fee_payer_signer_index: 0,
        buffer_signer_index: None,
        buffer_pubkey: None,
        buffer_authority_signer_index: 0,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        use_rpc: false,
        skip_feature_verification: false,
    });
    config.output_format = OutputFormat::JsonCompact;

    if enable_feature {
        let response = process_command(&config).await;
        assert!(response.is_ok());
    } else {
        expect_command_failure(
            &config,
            "Program contains a syscall from a deactivated feature",
            "ELF error: ELF error: Unresolved symbol (sol_alt_bn128_group_op) at instruction #49 \
             (ELF file offset 0x188)",
        )
        .await;

        // If we bypass the verification, there should be no error
        config.command = CliCommand::Program(ProgramCliCommand::WriteBuffer {
            program_location: program_path.to_str().unwrap().to_string(),
            fee_payer_signer_index: 0,
            buffer_signer_index: None,
            buffer_pubkey: None,
            buffer_authority_signer_index: 0,
            max_len: None,
            skip_fee_check: false,
            compute_unit_price: None,
            max_sign_attempts: 5,
            use_rpc: false,
            skip_feature_verification: true,
        });

        // When we skip verification, we won't fail
        let response = process_command(&config).await;
        assert!(response.is_ok());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cli_program_set_buffer_authority() {
    agave_logger::setup();

    let mut noop_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    noop_path.push("tests");
    noop_path.push("fixtures");
    noop_path.push("noop");
    noop_path.set_extension("so");

    let mint_keypair = Keypair::new();
    let test_validator = test_validator_genesis(&mint_keypair)
        .start_async_with_mint_address(&mint_keypair, SocketAddrSpace::Unspecified)
        .await
        .expect("validator start failed");

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    let rpc_client = setup_rpc_client(&mut config);

    let mut file = File::open(noop_path.to_str().unwrap()).unwrap();
    let mut program_data = Vec::new();
    file.read_to_end(&mut program_data).unwrap();
    let max_len = program_data.len();
    let minimum_balance_for_buffer = rpc_client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_programdata(
            max_len,
        ))
        .await
        .unwrap();

    let keypair = Keypair::new();
    config.signers = vec![&keypair];
    config.command = CliCommand::Airdrop {
        pubkey: None,
        lamports: 100 * minimum_balance_for_buffer,
    };
    process_command(&config).await.unwrap();

    // Write a buffer
    let buffer_keypair = Keypair::new();
    config.signers = vec![&keypair, &buffer_keypair];
    config.command = CliCommand::Program(ProgramCliCommand::WriteBuffer {
        program_location: noop_path.to_str().unwrap().to_string(),
        fee_payer_signer_index: 0,
        buffer_signer_index: Some(1),
        buffer_pubkey: Some(buffer_keypair.pubkey()),
        buffer_authority_signer_index: 0,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        use_rpc: false,
        skip_feature_verification: true,
    });
    process_command(&config).await.unwrap();
    let buffer_account = rpc_client
        .get_account(&buffer_keypair.pubkey())
        .await
        .unwrap();
    if let UpgradeableLoaderState::Buffer { authority_address } = buffer_account.state().unwrap() {
        assert_eq!(authority_address, Some(keypair.pubkey()));
    } else {
        panic!("not a buffer account");
    }

    // Set new buffer authority
    let new_buffer_authority = Keypair::new();
    config.signers = vec![&keypair, &buffer_keypair];
    config.command = CliCommand::Program(ProgramCliCommand::SetBufferAuthority {
        buffer_pubkey: buffer_keypair.pubkey(),
        buffer_authority_index: Some(0),
        new_buffer_authority: new_buffer_authority.pubkey(),
    });
    config.output_format = OutputFormat::JsonCompact;
    let response = process_command(&config).await;
    let json: Value = serde_json::from_str(&response.unwrap()).unwrap();
    let new_buffer_authority_str = json
        .as_object()
        .unwrap()
        .get("authority")
        .unwrap()
        .as_str()
        .unwrap();
    assert_eq!(
        Pubkey::from_str(new_buffer_authority_str).unwrap(),
        new_buffer_authority.pubkey()
    );
    let buffer_account = rpc_client
        .get_account(&buffer_keypair.pubkey())
        .await
        .unwrap();
    if let UpgradeableLoaderState::Buffer { authority_address } = buffer_account.state().unwrap() {
        assert_eq!(authority_address, Some(new_buffer_authority.pubkey()));
    } else {
        panic!("not a buffer account");
    }

    // Attempt to deploy program from buffer using previous authority (should fail)
    config.signers = vec![&keypair, &buffer_keypair];
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: None,
        program_pubkey: None,
        buffer_signer_index: None,
        buffer_pubkey: Some(buffer_keypair.pubkey()),
        upgrade_authority_signer_index: 0,
        is_final: false,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: true,
        use_rpc: false,
        skip_feature_verification: true,
    });
    config.output_format = OutputFormat::JsonCompact;
    expect_command_failure(
        &config,
        "Deployment with an old authority should fail",
        &format!(
            "Buffer's authority Some({}) does not match authority provided {}",
            new_buffer_authority.pubkey(),
            keypair.pubkey(),
        ),
    )
    .await;

    // Set buffer authority to the buffer identity (it's a common way for program devs to do so)
    config.signers = vec![&keypair, &new_buffer_authority];
    config.command = CliCommand::Program(ProgramCliCommand::SetBufferAuthority {
        buffer_pubkey: buffer_keypair.pubkey(),
        buffer_authority_index: Some(1),
        new_buffer_authority: buffer_keypair.pubkey(),
    });
    let response = process_command(&config).await;
    let json: Value = serde_json::from_str(&response.unwrap()).unwrap();
    let buffer_authority_str = json
        .as_object()
        .unwrap()
        .get("authority")
        .unwrap()
        .as_str()
        .unwrap();
    assert_eq!(
        Pubkey::from_str(buffer_authority_str).unwrap(),
        buffer_keypair.pubkey()
    );
    let buffer_account = rpc_client
        .get_account(&buffer_keypair.pubkey())
        .await
        .unwrap();
    if let UpgradeableLoaderState::Buffer { authority_address } = buffer_account.state().unwrap() {
        assert_eq!(authority_address, Some(buffer_keypair.pubkey()));
    } else {
        panic!("not a buffer account");
    }

    // Deploy from buffer using proper(new) buffer authority
    config.signers = vec![&keypair, &buffer_keypair];
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: None,
        program_pubkey: None,
        buffer_signer_index: None,
        buffer_pubkey: Some(buffer_keypair.pubkey()),
        upgrade_authority_signer_index: 1,
        is_final: false,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: true,
        use_rpc: false,
        skip_feature_verification: true,
    });
    config.output_format = OutputFormat::JsonCompact;
    process_command(&config).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cli_program_mismatch_buffer_authority() {
    agave_logger::setup();

    let mut noop_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    noop_path.push("tests");
    noop_path.push("fixtures");
    noop_path.push("noop");
    noop_path.set_extension("so");

    let mint_keypair = Keypair::new();
    let test_validator = test_validator_genesis(&mint_keypair)
        .start_async_with_mint_address(&mint_keypair, SocketAddrSpace::Unspecified)
        .await
        .expect("validator start failed");

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    let rpc_client = setup_rpc_client(&mut config);

    let mut file = File::open(noop_path.to_str().unwrap()).unwrap();
    let mut program_data = Vec::new();
    file.read_to_end(&mut program_data).unwrap();
    let max_len = program_data.len();
    let minimum_balance_for_buffer = rpc_client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_programdata(
            max_len,
        ))
        .await
        .unwrap();

    let keypair = Keypair::new();
    config.signers = vec![&keypair];
    config.command = CliCommand::Airdrop {
        pubkey: None,
        lamports: 100 * minimum_balance_for_buffer,
    };
    process_command(&config).await.unwrap();

    // Write a buffer
    let buffer_authority = Keypair::new();
    let buffer_keypair = Keypair::new();
    config.signers = vec![&keypair, &buffer_keypair, &buffer_authority];
    config.command = CliCommand::Program(ProgramCliCommand::WriteBuffer {
        program_location: noop_path.to_str().unwrap().to_string(),
        fee_payer_signer_index: 0,
        buffer_signer_index: Some(1),
        buffer_pubkey: Some(buffer_keypair.pubkey()),
        buffer_authority_signer_index: 2,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        use_rpc: false,
        skip_feature_verification: true,
    });
    process_command(&config).await.unwrap();
    let buffer_account = rpc_client
        .get_account(&buffer_keypair.pubkey())
        .await
        .unwrap();
    if let UpgradeableLoaderState::Buffer { authority_address } = buffer_account.state().unwrap() {
        assert_eq!(authority_address, Some(buffer_authority.pubkey()));
    } else {
        panic!("not a buffer account");
    }

    // Attempt to deploy with mismatched authority
    let upgrade_authority = Keypair::new();
    config.signers = vec![&keypair, &upgrade_authority];
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: None,
        program_pubkey: None,
        buffer_signer_index: None,
        buffer_pubkey: Some(buffer_keypair.pubkey()),
        upgrade_authority_signer_index: 1,
        is_final: true,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: true,
        use_rpc: false,
        skip_feature_verification: true,
    });
    expect_command_failure(
        &config,
        "Deployment with an invalid authority should fail",
        &format!(
            "Buffer's authority Some({}) does not match authority provided {}",
            buffer_authority.pubkey(),
            upgrade_authority.pubkey(),
        ),
    )
    .await;

    // Attempt to deploy matched authority
    config.signers = vec![&keypair, &buffer_authority];
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: None,
        program_pubkey: None,
        buffer_signer_index: None,
        buffer_pubkey: Some(buffer_keypair.pubkey()),
        upgrade_authority_signer_index: 1,
        is_final: true,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: true,
        use_rpc: false,
        skip_feature_verification: true,
    });
    process_command(&config).await.unwrap();
}

// Assume fee payer will be either online signer or offline signer (could be completely
// separate signer too, but that option is unlikely to be chosen often, so don't bother
// testing for it), we want to test for most common choices.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[test_case(true; "offline signer will be fee payer")]
#[test_case(false; "online signer will be fee payer")]
async fn test_cli_program_deploy_with_offline_signing(use_offline_signer_as_fee_payer: bool) {
    agave_logger::setup();

    let mut noop_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    noop_path.push("tests");
    noop_path.push("fixtures");
    noop_path.push("noop");
    noop_path.set_extension("so");

    let mut noop_large_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    noop_large_path.push("tests");
    noop_large_path.push("fixtures");
    noop_large_path.push("noop_large");
    noop_large_path.set_extension("so");

    let mint_keypair = Keypair::new();
    let test_validator = test_validator_genesis(&mint_keypair)
        .start_async_with_mint_address(&mint_keypair, SocketAddrSpace::Unspecified)
        .await
        .expect("validator start failed");

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    let rpc_client = setup_rpc_client(&mut config);

    let blockhash = rpc_client.get_latest_blockhash().await.unwrap();

    let mut file = File::open(noop_large_path.to_str().unwrap()).unwrap();
    let mut large_program_data = Vec::new();
    file.read_to_end(&mut large_program_data).unwrap();
    let max_program_data_len = large_program_data.len();
    let minimum_balance_for_large_buffer = rpc_client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_programdata(
            max_program_data_len,
        ))
        .await
        .unwrap();

    let online_signer = Keypair::new();
    let online_signer_identity = NullSigner::new(&online_signer.pubkey());
    let offline_signer = Keypair::new();
    let buffer_signer = Keypair::new();
    // Typically, keypair for program signer should be different from online signer or
    // offline signer keypairs.
    let program_signer = Keypair::new();

    config.command = CliCommand::Airdrop {
        pubkey: None,
        lamports: 100 * minimum_balance_for_large_buffer, // gotta be enough for this test
    };
    config.signers = vec![&online_signer];
    process_command(&config).await.unwrap();
    config.command = CliCommand::Airdrop {
        pubkey: None,
        lamports: 100 * minimum_balance_for_large_buffer, // gotta be enough for this test
    };
    config.signers = vec![&offline_signer];
    process_command(&config).await.unwrap();

    // Deploy upgradeable program with authority set to offline signer
    config.signers = vec![&online_signer, &offline_signer, &program_signer];
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: Some(2),
        program_pubkey: Some(program_signer.pubkey()),
        buffer_signer_index: None,
        buffer_pubkey: None,
        upgrade_authority_signer_index: 1, // must be offline signer for security reasons
        is_final: false,
        max_len: Some(max_program_data_len), // allows for larger program size with future upgrades
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: true,
        use_rpc: false,
        skip_feature_verification: true,
    });
    config.output_format = OutputFormat::JsonCompact;
    process_command(&config).await.unwrap();

    // Prepare buffer to upgrade deployed program to a larger program
    create_buffer_with_offline_authority(
        &rpc_client,
        &noop_large_path,
        &mut config,
        &online_signer,
        &offline_signer,
        &buffer_signer,
    )
    .await;

    // Offline sign-only with signature over "wrong" message (with different buffer)
    config.signers = vec![&offline_signer];
    let fee_payer_signer_index = if use_offline_signer_as_fee_payer {
        0 // offline signer
    } else {
        config.signers.push(&online_signer_identity); // can't (and won't) provide signature in --sign-only mode
        1 // online signer
    };
    config.command = CliCommand::Program(ProgramCliCommand::Upgrade {
        fee_payer_signer_index,
        program_pubkey: program_signer.pubkey(),
        buffer_pubkey: program_signer.pubkey(), // will ensure offline signature applies to wrong(different) message
        upgrade_authority_signer_index: 0,
        sign_only: true,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::new(Some(blockhash), true, None),
        skip_feature_verification: true,
    });
    config.output_format = OutputFormat::JsonCompact;
    let sig_response = process_command(&config).await.unwrap();
    let sign_only = parse_sign_only_reply_string(&sig_response);
    let offline_pre_signer = sign_only.presigner_of(&offline_signer.pubkey()).unwrap();
    // Attempt to deploy from buffer using signature over wrong(different) message (should fail)
    config.signers = vec![&offline_pre_signer, &program_signer];
    let fee_payer_signer_index = if use_offline_signer_as_fee_payer {
        0 // offline signer
    } else {
        config.signers.push(&online_signer); // can provide signature when not in --sign-only mode
        2 // online signer
    };
    config.command = CliCommand::Program(ProgramCliCommand::Upgrade {
        fee_payer_signer_index,
        program_pubkey: program_signer.pubkey(),
        buffer_pubkey: buffer_signer.pubkey(),
        upgrade_authority_signer_index: 0,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::new(Some(blockhash), true, None),
        skip_feature_verification: true,
    });
    config.output_format = OutputFormat::JsonCompact;
    expect_command_failure(
        &config,
        "Signature becomes invalid if the buffer is modified",
        "presigner error",
    )
    .await;

    // Offline sign-only with online signer as fee payer (correct signature for program upgrade)
    config.signers = vec![&offline_signer];
    let fee_payer_signer_index = if use_offline_signer_as_fee_payer {
        0 // offline signer
    } else {
        config.signers.push(&online_signer_identity); // can't (and won't) provide signature in --sign-only mode
        1 // online signer
    };
    config.command = CliCommand::Program(ProgramCliCommand::Upgrade {
        fee_payer_signer_index,
        program_pubkey: program_signer.pubkey(),
        buffer_pubkey: buffer_signer.pubkey(),
        upgrade_authority_signer_index: 0,
        sign_only: true,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::new(Some(blockhash), true, None),
        skip_feature_verification: true,
    });
    config.output_format = OutputFormat::JsonCompact;
    let sig_response = process_command(&config).await.unwrap();
    let sign_only = parse_sign_only_reply_string(&sig_response);
    let offline_pre_signer = sign_only.presigner_of(&offline_signer.pubkey()).unwrap();
    // Attempt to deploy from buffer using signature over correct message (should succeed)
    config.signers = vec![&offline_pre_signer, &program_signer];
    let fee_payer_signer_index = if use_offline_signer_as_fee_payer {
        0 // offline signer
    } else {
        config.signers.push(&online_signer); // can provide signature when not in --sign-only mode
        2 // online signer
    };
    config.command = CliCommand::Program(ProgramCliCommand::Upgrade {
        fee_payer_signer_index,
        program_pubkey: program_signer.pubkey(),
        buffer_pubkey: buffer_signer.pubkey(),
        upgrade_authority_signer_index: 0,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::new(Some(blockhash), true, None),
        skip_feature_verification: true,
    });
    config.output_format = OutputFormat::JsonCompact;
    process_command(&config).await.unwrap();
    let (programdata_pubkey, _) = Pubkey::find_program_address(
        &[program_signer.pubkey().as_ref()],
        &bpf_loader_upgradeable::id(),
    );
    let programdata_account = rpc_client.get_account(&programdata_pubkey).await.unwrap();
    assert_eq!(
        programdata_account.lamports,
        minimum_balance_for_large_buffer
    );
    assert_eq!(programdata_account.owner, bpf_loader_upgradeable::id());
    assert!(!programdata_account.executable);
    assert_eq!(
        programdata_account.data[UpgradeableLoaderState::size_of_programdata_metadata()..],
        large_program_data[..]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cli_program_show() {
    agave_logger::setup();

    let mut noop_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    noop_path.push("tests");
    noop_path.push("fixtures");
    noop_path.push("noop");
    noop_path.set_extension("so");

    let mint_keypair = Keypair::new();
    let test_validator = test_validator_genesis(&mint_keypair)
        .start_async_with_mint_address(&mint_keypair, SocketAddrSpace::Unspecified)
        .await
        .expect("validator start failed");

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    let rpc_client = setup_rpc_client(&mut config);

    let mut file = File::open(noop_path.to_str().unwrap()).unwrap();
    let mut program_data = Vec::new();
    file.read_to_end(&mut program_data).unwrap();
    let max_len = program_data.len();
    let minimum_balance_for_buffer = rpc_client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_programdata(
            max_len,
        ))
        .await
        .unwrap();

    let keypair = Keypair::new();
    config.output_format = OutputFormat::Json;

    // Airdrop
    config.signers = vec![&keypair];
    config.command = CliCommand::Airdrop {
        pubkey: None,
        lamports: 100 * minimum_balance_for_buffer,
    };
    process_command(&config).await.unwrap();

    // Write a buffer
    let buffer_keypair = Keypair::new();
    let authority_keypair = Keypair::new();
    config.signers = vec![&keypair, &buffer_keypair, &authority_keypair];
    config.command = CliCommand::Program(ProgramCliCommand::WriteBuffer {
        program_location: noop_path.to_str().unwrap().to_string(),
        fee_payer_signer_index: 0,
        buffer_signer_index: Some(1),
        buffer_pubkey: Some(buffer_keypair.pubkey()),
        buffer_authority_signer_index: 2,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        use_rpc: false,
        skip_feature_verification: true,
    });
    process_command(&config).await.unwrap();

    // Verify show
    config.signers = vec![&keypair];
    config.command = CliCommand::Program(ProgramCliCommand::Show {
        account_pubkey: Some(buffer_keypair.pubkey()),
        authority_pubkey: keypair.pubkey(),
        get_programs: false,
        get_buffers: false,
        all: false,
        use_lamports_unit: false,
    });
    let response = process_command(&config).await;
    let json: Value = serde_json::from_str(&response.unwrap()).unwrap();
    let address_str = json
        .as_object()
        .unwrap()
        .get("address")
        .unwrap()
        .as_str()
        .unwrap();
    assert_eq!(
        buffer_keypair.pubkey(),
        Pubkey::from_str(address_str).unwrap()
    );
    let authority_str = json
        .as_object()
        .unwrap()
        .get("authority")
        .unwrap()
        .as_str()
        .unwrap();
    assert_eq!(
        authority_keypair.pubkey(),
        Pubkey::from_str(authority_str).unwrap()
    );
    let data_len = json
        .as_object()
        .unwrap()
        .get("dataLen")
        .unwrap()
        .as_u64()
        .unwrap();
    assert_eq!(max_len, data_len as usize);

    // Deploy
    let program_keypair = Keypair::new();
    config.signers = vec![&keypair, &authority_keypair, &program_keypair];
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: Some(2),
        program_pubkey: Some(program_keypair.pubkey()),
        buffer_signer_index: None,
        buffer_pubkey: None,
        upgrade_authority_signer_index: 1,
        is_final: false,
        max_len: Some(max_len),
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        auto_extend: true,
        use_rpc: false,
        skip_feature_verification: true,
    });
    config.output_format = OutputFormat::JsonCompact;
    let min_slot = rpc_client.get_slot().await.unwrap();
    process_command(&config).await.unwrap();
    let max_slot = rpc_client.get_slot().await.unwrap();

    // Verify show
    config.signers = vec![&keypair];
    config.command = CliCommand::Program(ProgramCliCommand::Show {
        account_pubkey: Some(program_keypair.pubkey()),
        authority_pubkey: keypair.pubkey(),
        get_programs: false,
        get_buffers: false,
        all: false,
        use_lamports_unit: false,
    });
    let response = process_command(&config).await;
    let json: Value = serde_json::from_str(&response.unwrap()).unwrap();
    let address_str = json
        .as_object()
        .unwrap()
        .get("programId")
        .unwrap()
        .as_str()
        .unwrap();
    assert_eq!(
        program_keypair.pubkey(),
        Pubkey::from_str(address_str).unwrap()
    );
    let programdata_address_str = json
        .as_object()
        .unwrap()
        .get("programdataAddress")
        .unwrap()
        .as_str()
        .unwrap();
    let (programdata_pubkey, _) = Pubkey::find_program_address(
        &[program_keypair.pubkey().as_ref()],
        &bpf_loader_upgradeable::id(),
    );
    assert_eq!(
        programdata_pubkey,
        Pubkey::from_str(programdata_address_str).unwrap()
    );
    let authority_str = json
        .as_object()
        .unwrap()
        .get("authority")
        .unwrap()
        .as_str()
        .unwrap();
    assert_eq!(
        authority_keypair.pubkey(),
        Pubkey::from_str(authority_str).unwrap()
    );
    let deployed_slot = json
        .as_object()
        .unwrap()
        .get("lastDeploySlot")
        .unwrap()
        .as_u64()
        .unwrap();
    assert!(deployed_slot >= min_slot);
    assert!(deployed_slot <= max_slot);
    let data_len = json
        .as_object()
        .unwrap()
        .get("dataLen")
        .unwrap()
        .as_u64()
        .unwrap();
    assert_eq!(max_len, data_len as usize);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cli_program_dump() {
    agave_logger::setup();

    let mut noop_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    noop_path.push("tests");
    noop_path.push("fixtures");
    noop_path.push("noop");
    noop_path.set_extension("so");

    let mint_keypair = Keypair::new();
    let test_validator = test_validator_genesis(&mint_keypair)
        .start_async_with_mint_address(&mint_keypair, SocketAddrSpace::Unspecified)
        .await
        .expect("validator start failed");

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    let rpc_client = setup_rpc_client(&mut config);

    let mut file = File::open(noop_path.to_str().unwrap()).unwrap();
    let mut program_data = Vec::new();
    file.read_to_end(&mut program_data).unwrap();
    let max_len = program_data.len();
    let minimum_balance_for_buffer = rpc_client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_programdata(
            max_len,
        ))
        .await
        .unwrap();

    let keypair = Keypair::new();
    config.output_format = OutputFormat::Json;

    // Airdrop
    config.signers = vec![&keypair];
    config.command = CliCommand::Airdrop {
        pubkey: None,
        lamports: 100 * minimum_balance_for_buffer,
    };
    process_command(&config).await.unwrap();

    // Write a buffer
    let buffer_keypair = Keypair::new();
    let authority_keypair = Keypair::new();
    config.signers = vec![&keypair, &buffer_keypair, &authority_keypair];
    config.command = CliCommand::Program(ProgramCliCommand::WriteBuffer {
        program_location: noop_path.to_str().unwrap().to_string(),
        fee_payer_signer_index: 0,
        buffer_signer_index: Some(1),
        buffer_pubkey: Some(buffer_keypair.pubkey()),
        buffer_authority_signer_index: 2,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        use_rpc: false,
        skip_feature_verification: true,
    });
    process_command(&config).await.unwrap();

    // Verify dump
    let mut out_file = {
        let current_exe = env::current_exe().unwrap();
        PathBuf::from(current_exe.parent().unwrap().parent().unwrap())
    };
    out_file.set_file_name("out.txt");
    config.signers = vec![&keypair];
    config.command = CliCommand::Program(ProgramCliCommand::Dump {
        account_pubkey: Some(buffer_keypair.pubkey()),
        output_location: out_file.clone().into_os_string().into_string().unwrap(),
    });
    process_command(&config).await.unwrap();

    let mut file = File::open(out_file).unwrap();
    let mut out_data = Vec::new();
    file.read_to_end(&mut out_data).unwrap();
    assert_eq!(program_data.len(), out_data.len());
    for i in 0..program_data.len() {
        assert_eq!(program_data[i], out_data[i]);
    }
}

async fn create_buffer_with_offline_authority<'a>(
    rpc_client: &RpcClient,
    program_path: &Path,
    config: &mut CliConfig<'a>,
    online_signer: &'a Keypair,
    offline_signer: &'a Keypair,
    buffer_signer: &'a Keypair,
) {
    // Write a buffer
    config.signers = vec![online_signer, buffer_signer];
    config.command = CliCommand::Program(ProgramCliCommand::WriteBuffer {
        program_location: program_path.to_str().unwrap().to_string(),
        fee_payer_signer_index: 0,
        buffer_signer_index: Some(1),
        buffer_pubkey: Some(buffer_signer.pubkey()),
        buffer_authority_signer_index: 0,
        max_len: None,
        skip_fee_check: false,
        compute_unit_price: None,
        max_sign_attempts: 5,
        use_rpc: false,
        skip_feature_verification: true,
    });
    process_command(config).await.unwrap();
    let buffer_account = rpc_client
        .get_account(&buffer_signer.pubkey())
        .await
        .unwrap();
    if let UpgradeableLoaderState::Buffer { authority_address } = buffer_account.state().unwrap() {
        assert_eq!(authority_address, Some(online_signer.pubkey()));
    } else {
        panic!("not a buffer account");
    }

    // Set buffer authority to offline signer
    config.signers = vec![online_signer];
    config.command = CliCommand::Program(ProgramCliCommand::SetBufferAuthority {
        buffer_pubkey: buffer_signer.pubkey(),
        buffer_authority_index: Some(0),
        new_buffer_authority: offline_signer.pubkey(),
    });
    config.output_format = OutputFormat::JsonCompact;
    process_command(config).await.unwrap();
    let buffer_account = rpc_client
        .get_account(&buffer_signer.pubkey())
        .await
        .unwrap();
    if let UpgradeableLoaderState::Buffer { authority_address } = buffer_account.state().unwrap() {
        assert_eq!(authority_address, Some(offline_signer.pubkey()));
    } else {
        panic!("not a buffer account");
    }
}

#[allow(clippy::assertions_on_constants)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[test_case(None, false; "default")]
#[test_case(Some(10), false; "with_compute_unit_price")]
#[test_case(None, true; "use_rpc")]
async fn test_cli_program_deploy_with_args(compute_unit_price: Option<u64>, use_rpc: bool) {
    let mut noop_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    noop_path.push("tests");
    noop_path.push("fixtures");
    noop_path.push("noop");
    noop_path.set_extension("so");

    let mint_keypair = Keypair::new();
    let faucet_addr = run_local_faucet_with_unique_port_for_tests(mint_keypair.insecure_clone());
    let test_validator = TestValidatorGenesis::default()
        .fee_rate_governor(FeeRateGovernor::new(0, 0))
        .rent(Rent {
            lamports_per_byte_year: 1,
            exemption_threshold: 1.0,
            ..Rent::default()
        })
        .rpc_config(JsonRpcConfig {
            enable_rpc_transaction_history: true,
            faucet_addr: Some(faucet_addr),
            ..JsonRpcConfig::default_for_test()
        })
        .start_async_with_mint_address(&mint_keypair, SocketAddrSpace::Unspecified)
        .await
        .expect("validator start failed");

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    let rpc_client = setup_rpc_client(&mut config);

    let mut file = File::open(noop_path.to_str().unwrap()).unwrap();
    let mut program_data = Vec::new();
    file.read_to_end(&mut program_data).unwrap();
    let max_len = program_data.len();
    let minimum_balance_for_programdata = rpc_client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_programdata(
            max_len,
        ))
        .await
        .unwrap();
    let minimum_balance_for_program = rpc_client
        .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_program())
        .await
        .unwrap();
    let upgrade_authority = Keypair::new();

    let keypair = Keypair::new();
    config.signers = vec![&keypair];
    config.command = CliCommand::Airdrop {
        pubkey: None,
        lamports: 100 * minimum_balance_for_programdata + minimum_balance_for_program,
    };
    process_command(&config).await.unwrap();

    // Deploy the upgradeable program with specified program_id
    let program_keypair = Keypair::new();
    config.signers = vec![&keypair, &upgrade_authority, &program_keypair];
    config.command = CliCommand::Program(ProgramCliCommand::Deploy {
        program_location: Some(noop_path.to_str().unwrap().to_string()),
        fee_payer_signer_index: 0,
        program_signer_index: Some(2),
        program_pubkey: Some(program_keypair.pubkey()),
        buffer_signer_index: None,
        buffer_pubkey: None,
        upgrade_authority_signer_index: 1,
        is_final: false,
        max_len: Some(max_len),
        skip_fee_check: false,
        compute_unit_price,
        max_sign_attempts: 5,
        auto_extend: true,
        use_rpc,
        skip_feature_verification: true,
    });
    config.output_format = OutputFormat::JsonCompact;
    let response = process_command(&config).await;
    let json: Value = serde_json::from_str(&response.unwrap()).unwrap();
    let program_pubkey_str = json
        .as_object()
        .unwrap()
        .get("programId")
        .unwrap()
        .as_str()
        .unwrap();
    assert_eq!(
        program_keypair.pubkey(),
        Pubkey::from_str(program_pubkey_str).unwrap()
    );
    let program_account = rpc_client
        .get_account(&program_keypair.pubkey())
        .await
        .unwrap();
    assert_eq!(program_account.lamports, minimum_balance_for_program);
    assert_eq!(program_account.owner, bpf_loader_upgradeable::id());
    assert!(program_account.executable);
    let signature_statuses = rpc_client
        .get_signatures_for_address_with_config(
            &keypair.pubkey(),
            GetConfirmedSignaturesForAddress2Config {
                commitment: Some(CommitmentConfig::confirmed()),
                ..GetConfirmedSignaturesForAddress2Config::default()
            },
        )
        .await
        .unwrap();
    let signatures: Vec<_> = signature_statuses
        .into_iter()
        .rev()
        .map(|status| Signature::from_str(&status.signature).unwrap())
        .collect();

    async fn fetch_and_decode_transaction(
        rpc_client: &RpcClient,
        signature: &Signature,
    ) -> Transaction {
        rpc_client
            .get_transaction_with_config(
                signature,
                RpcTransactionConfig {
                    encoding: Some(UiTransactionEncoding::Base64),
                    commitment: Some(CommitmentConfig::confirmed()),
                    ..RpcTransactionConfig::default()
                },
            )
            .await
            .unwrap()
            .transaction
            .transaction
            .decode()
            .unwrap()
            .into_legacy_transaction()
            .unwrap()
    }

    assert!(signatures.len() >= 4);
    let initial_tx = fetch_and_decode_transaction(&rpc_client, &signatures[1]).await;
    let write_tx = fetch_and_decode_transaction(&rpc_client, &signatures[2]).await;
    let final_tx = fetch_and_decode_transaction(&rpc_client, signatures.last().unwrap()).await;

    if let Some(compute_unit_price) = compute_unit_price {
        for tx in [&initial_tx, &write_tx, &final_tx] {
            let ix_len = tx.message.instructions.len();
            for i in [1, 2] {
                assert_eq!(
                    tx.message.instructions[ix_len - i].program_id(&tx.message.account_keys),
                    &compute_budget::id()
                );
            }

            assert_matches!(
                try_from_slice_unchecked(&tx.message.instructions[ix_len - 2].data),
                Ok(ComputeBudgetInstruction::SetComputeUnitPrice(price)) if price == compute_unit_price
            );
        }

        assert_matches!(
            try_from_slice_unchecked(&initial_tx.message.instructions.last().unwrap().data),
            Ok(ComputeBudgetInstruction::SetComputeUnitLimit(2820))
        );
        assert_matches!(
            try_from_slice_unchecked(&write_tx.message.instructions.last().unwrap().data),
            Ok(ComputeBudgetInstruction::SetComputeUnitLimit(2670))
        );
        assert_matches!(
            try_from_slice_unchecked(&final_tx.message.instructions.last().unwrap().data),
            Ok(ComputeBudgetInstruction::SetComputeUnitLimit(2970))
        );
    } else {
        assert_eq!(
            initial_tx.message.instructions[0].program_id(&initial_tx.message.account_keys),
            &system_program::id()
        );
        assert_eq!(
            write_tx.message.instructions[0].program_id(&write_tx.message.account_keys),
            &bpf_loader_upgradeable::id()
        );
        assert_eq!(
            final_tx.message.instructions[0].program_id(&final_tx.message.account_keys),
            &system_program::id()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cli_program_v4() {
    let mut noop_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    noop_path.push("tests");
    noop_path.push("fixtures");
    noop_path.push("noop");
    noop_path.set_extension("so");

    let mint_keypair = Keypair::new();
    let test_validator = test_validator_genesis(&mint_keypair)
        .start_async_with_mint_address(&mint_keypair, SocketAddrSpace::Unspecified)
        .await
        .expect("validator start failed");

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    let rpc_client = setup_rpc_client(&mut config);

    let payer_keypair = Keypair::new();
    let upgrade_authority = Keypair::new();
    let program_keypair = Keypair::new();
    let buffer_keypair = Keypair::new();
    config.signers = vec![
        &payer_keypair,
        &upgrade_authority,
        &program_keypair,
        &buffer_keypair,
    ];
    config.command = CliCommand::Airdrop {
        pubkey: None,
        lamports: 10000000,
    };
    process_command(&config).await.unwrap();
    config.command = CliCommand::Airdrop {
        pubkey: Some(program_keypair.pubkey()),
        lamports: 1000,
    };
    process_command(&config).await.unwrap();

    // Initial deployment
    config.output_format = OutputFormat::JsonCompact;
    config.command = CliCommand::ProgramV4(ProgramV4CliCommand::Deploy {
        additional_cli_config: AdditionalCliConfig::default(),
        program_address: program_keypair.pubkey(),
        buffer_address: None,
        upload_signer_index: Some(2),
        authority_signer_index: 1,
        path_to_elf: Some(noop_path.to_str().unwrap().to_string()),
        upload_range: None..None,
    });
    assert!(process_command(&config).await.is_ok());
    let program_account = rpc_client
        .get_account(&program_keypair.pubkey())
        .await
        .unwrap();
    assert_eq!(program_account.owner, loader_v4::id());
    assert!(program_account.executable);

    // Single-step redeployment without buffer
    config.command = CliCommand::ProgramV4(ProgramV4CliCommand::Deploy {
        additional_cli_config: AdditionalCliConfig::default(),
        program_address: program_keypair.pubkey(),
        buffer_address: None,
        upload_signer_index: None,
        authority_signer_index: 1,
        path_to_elf: Some(noop_path.to_str().unwrap().to_string()),
        upload_range: None..None,
    });
    assert!(process_command(&config).await.is_ok());
    let program_account = rpc_client
        .get_account(&program_keypair.pubkey())
        .await
        .unwrap();
    assert_eq!(program_account.owner, loader_v4::id());
    assert!(program_account.executable);

    // Single-step redeployment with buffer
    config.command = CliCommand::ProgramV4(ProgramV4CliCommand::Deploy {
        additional_cli_config: AdditionalCliConfig::default(),
        program_address: program_keypair.pubkey(),
        buffer_address: Some(buffer_keypair.pubkey()),
        upload_signer_index: Some(3),
        authority_signer_index: 1,
        path_to_elf: Some(noop_path.to_str().unwrap().to_string()),
        upload_range: None..None,
    });
    assert!(process_command(&config).await.is_ok());
    let program_account = rpc_client
        .get_account(&program_keypair.pubkey())
        .await
        .unwrap();
    assert_eq!(program_account.owner, loader_v4::id());
    assert!(program_account.executable);
    let _error = rpc_client
        .get_account(&buffer_keypair.pubkey())
        .await
        .unwrap_err();

    // Two-step redeployment with buffer
    config.command = CliCommand::ProgramV4(ProgramV4CliCommand::Deploy {
        additional_cli_config: AdditionalCliConfig::default(),
        program_address: buffer_keypair.pubkey(),
        buffer_address: Some(buffer_keypair.pubkey()),
        upload_signer_index: Some(3),
        authority_signer_index: 1,
        path_to_elf: Some(noop_path.to_str().unwrap().to_string()),
        upload_range: None..None,
    });
    assert!(process_command(&config).await.is_ok());
    let buffer_account = rpc_client
        .get_account(&buffer_keypair.pubkey())
        .await
        .unwrap();
    assert_eq!(buffer_account.owner, loader_v4::id());
    assert!(buffer_account.executable);
    config.command = CliCommand::ProgramV4(ProgramV4CliCommand::Deploy {
        additional_cli_config: AdditionalCliConfig::default(),
        program_address: program_keypair.pubkey(),
        buffer_address: Some(buffer_keypair.pubkey()),
        upload_signer_index: None,
        authority_signer_index: 1,
        path_to_elf: Some(noop_path.to_str().unwrap().to_string()),
        upload_range: None..None,
    });
    assert!(process_command(&config).await.is_ok());
    let program_account = rpc_client
        .get_account(&program_keypair.pubkey())
        .await
        .unwrap();
    assert_eq!(program_account.owner, loader_v4::id());
    assert!(program_account.executable);
    let _error = rpc_client
        .get_account(&buffer_keypair.pubkey())
        .await
        .unwrap_err();

    // Transfer authority over program
    config.command = CliCommand::ProgramV4(ProgramV4CliCommand::TransferAuthority {
        additional_cli_config: AdditionalCliConfig::default(),
        program_address: program_keypair.pubkey(),
        authority_signer_index: 1,
        new_authority_signer_index: 2,
    });
    assert!(process_command(&config).await.is_ok());
    let program_account = rpc_client
        .get_account(&program_keypair.pubkey())
        .await
        .unwrap();
    assert_eq!(program_account.owner, loader_v4::id());
    assert!(program_account.executable);

    // Close program
    config.command = CliCommand::ProgramV4(ProgramV4CliCommand::Retract {
        additional_cli_config: AdditionalCliConfig::default(),
        program_address: program_keypair.pubkey(),
        authority_signer_index: 2,
        close_program_entirely: true,
    });
    assert!(process_command(&config).await.is_ok());
    let _error = rpc_client
        .get_account(&program_keypair.pubkey())
        .await
        .unwrap_err();

    // Deployment at the closed address
    config.command = CliCommand::ProgramV4(ProgramV4CliCommand::Deploy {
        additional_cli_config: AdditionalCliConfig::default(),
        program_address: program_keypair.pubkey(),
        buffer_address: None,
        upload_signer_index: Some(2),
        authority_signer_index: 1,
        path_to_elf: Some(noop_path.to_str().unwrap().to_string()),
        upload_range: None..None,
    });
    assert!(process_command(&config).await.is_ok());
    let program_account = rpc_client
        .get_account(&program_keypair.pubkey())
        .await
        .unwrap();
    assert_eq!(program_account.owner, loader_v4::id());
    assert!(program_account.executable);

    // Finalize program
    config.command = CliCommand::ProgramV4(ProgramV4CliCommand::Finalize {
        additional_cli_config: AdditionalCliConfig::default(),
        program_address: program_keypair.pubkey(),
        authority_signer_index: 1,
        next_version_signer_index: 2,
    });
    assert!(process_command(&config).await.is_ok());
    let program_account = rpc_client
        .get_account(&program_keypair.pubkey())
        .await
        .unwrap();
    assert_eq!(program_account.owner, loader_v4::id());
    assert!(program_account.executable);
}
