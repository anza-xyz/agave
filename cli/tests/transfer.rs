#![allow(clippy::arithmetic_side_effects)]
use {
    solana_cli::{
        check_balance,
        cli::{process_command, request_and_confirm_airdrop, CliCommand, CliConfig},
        spend_utils::SpendAmount,
        test_utils::check_ready,
    },
    solana_cli_output::{parse_sign_only_reply_string, OutputFormat},
    solana_client::nonblocking::blockhash_query::Source,
    solana_commitment_config::CommitmentConfig,
    solana_compute_budget_interface::ComputeBudgetInstruction,
    solana_faucet::faucet::run_local_faucet_with_unique_port_for_tests,
    solana_fee_structure::FeeStructure,
    solana_keypair::{keypair_from_seed, Keypair},
    solana_message::Message,
    solana_native_token::LAMPORTS_PER_SOL,
    solana_net_utils::SocketAddrSpace,
    solana_nonce::state::State as NonceState,
    solana_pubkey::Pubkey,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_rpc_client_nonce_utils::nonblocking::blockhash_query::BlockhashQuery,
    solana_signer::{null_signer::NullSigner, Signer},
    solana_stake_interface as stake,
    solana_system_interface::instruction as system_instruction,
    solana_test_validator::TestValidator,
    test_case::test_case,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[test_case(true; "Skip Preflight")]
#[test_case(false; "Don`t skip Preflight")]
async fn test_transfer(skip_preflight: bool) {
    agave_logger::setup();
    let fee_one_sig = FeeStructure::default().get_max_fee(1, 0);
    let fee_two_sig = FeeStructure::default().get_max_fee(2, 0);
    let mint_keypair = Keypair::new();
    let faucet_addr = run_local_faucet_with_unique_port_for_tests(mint_keypair.insecure_clone());
    let test_validator = TestValidator::async_with_custom_fees(
        &mint_keypair,
        fee_one_sig,
        Some(faucet_addr),
        SocketAddrSpace::Unspecified,
    )
    .await;

    let rpc_client =
        RpcClient::new_with_commitment(test_validator.rpc_url(), CommitmentConfig::processed());

    let default_signer = Keypair::new();
    let default_offline_signer = Keypair::new();

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    config.signers = vec![&default_signer];
    config.send_transaction_config.skip_preflight = skip_preflight;

    let sender_pubkey = config.signers[0].pubkey();
    let recipient_pubkey = Pubkey::from([1u8; 32]);

    request_and_confirm_airdrop(&rpc_client, &config, &sender_pubkey, 5 * LAMPORTS_PER_SOL)
        .await
        .unwrap();
    check_balance!(5 * LAMPORTS_PER_SOL, &rpc_client, &sender_pubkey);
    check_balance!(0, &rpc_client, &recipient_pubkey);

    check_ready(&rpc_client).await;

    // Plain ole transfer
    config.command = CliCommand::Transfer {
        amount: SpendAmount::Some(LAMPORTS_PER_SOL),
        to: recipient_pubkey,
        from: 0,
        sign_only: false,
        dump_transaction_message: false,
        allow_unfunded_recipient: true,
        no_wait: false,
        blockhash_query: BlockhashQuery::Rpc(Source::Cluster),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        derived_address_seed: None,
        derived_address_program_id: None,
        compute_unit_price: None,
    };
    process_command(&config).await.unwrap();
    check_balance!(
        4 * LAMPORTS_PER_SOL - fee_one_sig,
        &rpc_client,
        &sender_pubkey
    );
    check_balance!(LAMPORTS_PER_SOL, &rpc_client, &recipient_pubkey);

    // Plain ole transfer, failure due to InsufficientFundsForSpendAndFee
    config.command = CliCommand::Transfer {
        amount: SpendAmount::Some(4 * LAMPORTS_PER_SOL),
        to: recipient_pubkey,
        from: 0,
        sign_only: false,
        dump_transaction_message: false,
        allow_unfunded_recipient: true,
        no_wait: false,
        blockhash_query: BlockhashQuery::Rpc(Source::Cluster),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        derived_address_seed: None,
        derived_address_program_id: None,
        compute_unit_price: None,
    };
    assert!(process_command(&config).await.is_err());
    check_balance!(
        4 * LAMPORTS_PER_SOL - fee_one_sig,
        &rpc_client,
        &sender_pubkey
    );
    check_balance!(LAMPORTS_PER_SOL, &rpc_client, &recipient_pubkey);

    let mut offline = CliConfig::recent_for_tests();
    offline.json_rpc_url = String::default();
    offline.signers = vec![&default_offline_signer];
    // Verify we cannot contact the cluster
    offline.command = CliCommand::ClusterVersion;
    process_command(&offline).await.unwrap_err();

    let offline_pubkey = offline.signers[0].pubkey();
    request_and_confirm_airdrop(&rpc_client, &offline, &offline_pubkey, LAMPORTS_PER_SOL)
        .await
        .unwrap();
    check_balance!(LAMPORTS_PER_SOL, &rpc_client, &offline_pubkey);

    // Offline transfer
    let blockhash = rpc_client.get_latest_blockhash().await.unwrap();
    offline.command = CliCommand::Transfer {
        amount: SpendAmount::Some(LAMPORTS_PER_SOL / 2),
        to: recipient_pubkey,
        from: 0,
        sign_only: true,
        dump_transaction_message: false,
        allow_unfunded_recipient: true,
        no_wait: false,
        blockhash_query: BlockhashQuery::Static(blockhash),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        derived_address_seed: None,
        derived_address_program_id: None,
        compute_unit_price: None,
    };
    offline.output_format = OutputFormat::JsonCompact;
    let sign_only_reply = process_command(&offline).await.unwrap();
    let sign_only = parse_sign_only_reply_string(&sign_only_reply);
    assert!(sign_only.has_all_signers());
    let offline_presigner = sign_only.presigner_of(&offline_pubkey).unwrap();
    config.signers = vec![&offline_presigner];
    config.command = CliCommand::Transfer {
        amount: SpendAmount::Some(LAMPORTS_PER_SOL / 2),
        to: recipient_pubkey,
        from: 0,
        sign_only: false,
        dump_transaction_message: false,
        allow_unfunded_recipient: true,
        no_wait: false,
        blockhash_query: BlockhashQuery::Validated(Source::Cluster, blockhash),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        derived_address_seed: None,
        derived_address_program_id: None,
        compute_unit_price: None,
    };
    process_command(&config).await.unwrap();
    check_balance!(
        LAMPORTS_PER_SOL / 2 - fee_one_sig,
        &rpc_client,
        &offline_pubkey
    );
    check_balance!(1_500_000_000, &rpc_client, &recipient_pubkey);

    // Create nonce account
    let nonce_account = keypair_from_seed(&[3u8; 32]).unwrap();
    let minimum_nonce_balance = rpc_client
        .get_minimum_balance_for_rent_exemption(NonceState::size())
        .await
        .unwrap();
    config.signers = vec![&default_signer, &nonce_account];
    config.command = CliCommand::CreateNonceAccount {
        nonce_account: 1,
        seed: None,
        nonce_authority: None,
        memo: None,
        amount: SpendAmount::Some(minimum_nonce_balance),
        compute_unit_price: None,
    };
    process_command(&config).await.unwrap();
    check_balance!(
        4 * LAMPORTS_PER_SOL - fee_one_sig - fee_two_sig - minimum_nonce_balance,
        &rpc_client,
        &sender_pubkey,
    );

    // Fetch nonce hash
    let nonce_hash = solana_rpc_client_nonce_utils::nonblocking::get_account_with_commitment(
        &rpc_client,
        &nonce_account.pubkey(),
        CommitmentConfig::processed(),
    )
    .await
    .and_then(|ref a| solana_rpc_client_nonce_utils::data_from_account(a))
    .unwrap()
    .blockhash();

    // Nonced transfer
    config.signers = vec![&default_signer];
    config.command = CliCommand::Transfer {
        amount: SpendAmount::Some(LAMPORTS_PER_SOL),
        to: recipient_pubkey,
        from: 0,
        sign_only: false,
        dump_transaction_message: false,
        allow_unfunded_recipient: true,
        no_wait: false,
        blockhash_query: BlockhashQuery::Validated(
            Source::NonceAccount(nonce_account.pubkey()),
            nonce_hash,
        ),
        nonce_account: Some(nonce_account.pubkey()),
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        derived_address_seed: None,
        derived_address_program_id: None,
        compute_unit_price: None,
    };
    process_command(&config).await.unwrap();
    check_balance!(
        3 * LAMPORTS_PER_SOL - 2 * fee_one_sig - fee_two_sig - minimum_nonce_balance,
        &rpc_client,
        &sender_pubkey,
    );
    check_balance!(2_500_000_000, &rpc_client, &recipient_pubkey);
    let new_nonce_hash = solana_rpc_client_nonce_utils::nonblocking::get_account_with_commitment(
        &rpc_client,
        &nonce_account.pubkey(),
        CommitmentConfig::processed(),
    )
    .await
    .and_then(|ref a| solana_rpc_client_nonce_utils::data_from_account(a))
    .unwrap()
    .blockhash();
    assert_ne!(nonce_hash, new_nonce_hash);

    // Assign nonce authority to offline
    config.signers = vec![&default_signer];
    config.command = CliCommand::AuthorizeNonceAccount {
        nonce_account: nonce_account.pubkey(),
        nonce_authority: 0,
        memo: None,
        new_authority: offline_pubkey,
        compute_unit_price: None,
    };
    process_command(&config).await.unwrap();
    check_balance!(
        3 * LAMPORTS_PER_SOL - 3 * fee_one_sig - fee_two_sig - minimum_nonce_balance,
        &rpc_client,
        &sender_pubkey,
    );

    // Fetch nonce hash
    let nonce_hash = solana_rpc_client_nonce_utils::nonblocking::get_account_with_commitment(
        &rpc_client,
        &nonce_account.pubkey(),
        CommitmentConfig::processed(),
    )
    .await
    .and_then(|ref a| solana_rpc_client_nonce_utils::data_from_account(a))
    .unwrap()
    .blockhash();

    // Offline, nonced transfer
    offline.signers = vec![&default_offline_signer];
    offline.command = CliCommand::Transfer {
        amount: SpendAmount::Some(400_000_000),
        to: recipient_pubkey,
        from: 0,
        sign_only: true,
        dump_transaction_message: false,
        allow_unfunded_recipient: true,
        no_wait: false,
        blockhash_query: BlockhashQuery::Static(nonce_hash),
        nonce_account: Some(nonce_account.pubkey()),
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        derived_address_seed: None,
        derived_address_program_id: None,
        compute_unit_price: None,
    };
    let sign_only_reply = process_command(&offline).await.unwrap();
    let sign_only = parse_sign_only_reply_string(&sign_only_reply);
    assert!(sign_only.has_all_signers());
    let offline_presigner = sign_only.presigner_of(&offline_pubkey).unwrap();
    config.signers = vec![&offline_presigner];
    config.command = CliCommand::Transfer {
        amount: SpendAmount::Some(400_000_000),
        to: recipient_pubkey,
        from: 0,
        sign_only: false,
        dump_transaction_message: false,
        allow_unfunded_recipient: true,
        no_wait: false,
        blockhash_query: BlockhashQuery::Validated(
            Source::NonceAccount(nonce_account.pubkey()),
            sign_only.blockhash,
        ),
        nonce_account: Some(nonce_account.pubkey()),
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        derived_address_seed: None,
        derived_address_program_id: None,
        compute_unit_price: None,
    };
    process_command(&config).await.unwrap();
    check_balance!(
        LAMPORTS_PER_SOL / 10 - 2 * fee_one_sig,
        &rpc_client,
        &offline_pubkey
    );
    check_balance!(2_900_000_000, &rpc_client, &recipient_pubkey);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_transfer_multisession_signing() {
    agave_logger::setup();
    let fee_one_sig = FeeStructure::default().get_max_fee(1, 0);
    let fee_two_sig = FeeStructure::default().get_max_fee(2, 0);
    let mint_keypair = Keypair::new();
    let faucet_addr = run_local_faucet_with_unique_port_for_tests(mint_keypair.insecure_clone());
    let test_validator = TestValidator::async_with_custom_fees(
        &mint_keypair,
        fee_one_sig,
        Some(faucet_addr),
        SocketAddrSpace::Unspecified,
    )
    .await;

    let to_pubkey = Pubkey::from([1u8; 32]);
    let offline_from_signer = keypair_from_seed(&[2u8; 32]).unwrap();
    let offline_fee_payer_signer = keypair_from_seed(&[3u8; 32]).unwrap();
    let from_null_signer = NullSigner::new(&offline_from_signer.pubkey());

    // Setup accounts
    let rpc_client =
        RpcClient::new_with_commitment(test_validator.rpc_url(), CommitmentConfig::processed());
    request_and_confirm_airdrop(
        &rpc_client,
        &CliConfig::recent_for_tests(),
        &offline_from_signer.pubkey(),
        43 * LAMPORTS_PER_SOL,
    )
    .await
    .unwrap();
    request_and_confirm_airdrop(
        &rpc_client,
        &CliConfig::recent_for_tests(),
        &offline_fee_payer_signer.pubkey(),
        LAMPORTS_PER_SOL + 2 * fee_two_sig,
    )
    .await
    .unwrap();
    check_balance!(
        43 * LAMPORTS_PER_SOL,
        &rpc_client,
        &offline_from_signer.pubkey(),
    );
    check_balance!(
        LAMPORTS_PER_SOL + 2 * fee_two_sig,
        &rpc_client,
        &offline_fee_payer_signer.pubkey(),
    );
    check_balance!(0, &rpc_client, &to_pubkey);

    check_ready(&rpc_client).await;

    let blockhash = rpc_client.get_latest_blockhash().await.unwrap();

    // Offline fee-payer signs first
    let mut fee_payer_config = CliConfig::recent_for_tests();
    fee_payer_config.json_rpc_url = String::default();
    fee_payer_config.signers = vec![&offline_fee_payer_signer, &from_null_signer];
    // Verify we cannot contact the cluster
    fee_payer_config.command = CliCommand::ClusterVersion;
    process_command(&fee_payer_config).await.unwrap_err();
    fee_payer_config.command = CliCommand::Transfer {
        amount: SpendAmount::Some(42 * LAMPORTS_PER_SOL),
        to: to_pubkey,
        from: 1,
        sign_only: true,
        dump_transaction_message: false,
        allow_unfunded_recipient: true,
        no_wait: false,
        blockhash_query: BlockhashQuery::Static(blockhash),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        derived_address_seed: None,
        derived_address_program_id: None,
        compute_unit_price: None,
    };
    fee_payer_config.output_format = OutputFormat::JsonCompact;
    let sign_only_reply = process_command(&fee_payer_config).await.unwrap();
    let sign_only = parse_sign_only_reply_string(&sign_only_reply);
    assert!(!sign_only.has_all_signers());
    let fee_payer_presigner = sign_only
        .presigner_of(&offline_fee_payer_signer.pubkey())
        .unwrap();

    // Now the offline fund source
    let mut from_config = CliConfig::recent_for_tests();
    from_config.json_rpc_url = String::default();
    from_config.signers = vec![&fee_payer_presigner, &offline_from_signer];
    // Verify we cannot contact the cluster
    from_config.command = CliCommand::ClusterVersion;
    process_command(&from_config).await.unwrap_err();
    from_config.command = CliCommand::Transfer {
        amount: SpendAmount::Some(42 * LAMPORTS_PER_SOL),
        to: to_pubkey,
        from: 1,
        sign_only: true,
        dump_transaction_message: false,
        allow_unfunded_recipient: true,
        no_wait: false,
        blockhash_query: BlockhashQuery::Static(blockhash),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        derived_address_seed: None,
        derived_address_program_id: None,
        compute_unit_price: None,
    };
    from_config.output_format = OutputFormat::JsonCompact;
    let sign_only_reply = process_command(&from_config).await.unwrap();
    let sign_only = parse_sign_only_reply_string(&sign_only_reply);
    assert!(sign_only.has_all_signers());
    let from_presigner = sign_only
        .presigner_of(&offline_from_signer.pubkey())
        .unwrap();

    // Finally submit to the cluster
    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    config.signers = vec![&fee_payer_presigner, &from_presigner];
    config.command = CliCommand::Transfer {
        amount: SpendAmount::Some(42 * LAMPORTS_PER_SOL),
        to: to_pubkey,
        from: 1,
        sign_only: false,
        dump_transaction_message: false,
        allow_unfunded_recipient: true,
        no_wait: false,
        blockhash_query: BlockhashQuery::Validated(Source::Cluster, blockhash),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        derived_address_seed: None,
        derived_address_program_id: None,
        compute_unit_price: None,
    };
    process_command(&config).await.unwrap();

    check_balance!(LAMPORTS_PER_SOL, &rpc_client, &offline_from_signer.pubkey(),);
    check_balance!(
        LAMPORTS_PER_SOL + fee_two_sig,
        &rpc_client,
        &offline_fee_payer_signer.pubkey(),
    );
    check_balance!(42 * LAMPORTS_PER_SOL, &rpc_client, &to_pubkey);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[test_case(None; "default")]
#[test_case(Some(100_000); "with_compute_unit_price")]
async fn test_transfer_all(compute_unit_price: Option<u64>) {
    agave_logger::setup();
    let lamports_per_signature = FeeStructure::default().get_max_fee(1, 0);
    let mint_keypair = Keypair::new();
    let faucet_addr = run_local_faucet_with_unique_port_for_tests(mint_keypair.insecure_clone());
    let test_validator = TestValidator::async_with_custom_fees(
        &mint_keypair,
        lamports_per_signature,
        Some(faucet_addr),
        SocketAddrSpace::Unspecified,
    )
    .await;

    let rpc_client =
        RpcClient::new_with_commitment(test_validator.rpc_url(), CommitmentConfig::processed());

    let default_signer = Keypair::new();
    let recipient_pubkey = Pubkey::from([1u8; 32]);

    let fee = {
        let mut instructions = vec![system_instruction::transfer(
            &default_signer.pubkey(),
            &recipient_pubkey,
            0,
        )];
        if let Some(compute_unit_price) = compute_unit_price {
            // This is brittle and will need to be updated if the compute unit
            // limit for the system program or compute budget program are changed,
            // or if they're converted to BPF.
            // See `solana_system_program::system_processor::DEFAULT_COMPUTE_UNITS`
            // and `solana_compute_budget_program::DEFAULT_COMPUTE_UNITS`
            instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(450));
            instructions.push(ComputeBudgetInstruction::set_compute_unit_price(
                compute_unit_price,
            ));
        }
        let blockhash = rpc_client.get_latest_blockhash().await.unwrap();
        let sample_message =
            Message::new_with_blockhash(&instructions, Some(&default_signer.pubkey()), &blockhash);
        rpc_client
            .get_fee_for_message(&sample_message)
            .await
            .unwrap()
    };

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    config.signers = vec![&default_signer];

    let sender_pubkey = config.signers[0].pubkey();

    request_and_confirm_airdrop(&rpc_client, &config, &sender_pubkey, 500_000)
        .await
        .unwrap();
    check_balance!(500_000, &rpc_client, &sender_pubkey);
    check_balance!(0, &rpc_client, &recipient_pubkey);

    check_ready(&rpc_client).await;

    // Plain ole transfer
    config.command = CliCommand::Transfer {
        amount: SpendAmount::All,
        to: recipient_pubkey,
        from: 0,
        sign_only: false,
        dump_transaction_message: false,
        allow_unfunded_recipient: true,
        no_wait: false,
        blockhash_query: BlockhashQuery::Rpc(Source::Cluster),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        derived_address_seed: None,
        derived_address_program_id: None,
        compute_unit_price,
    };
    process_command(&config).await.unwrap();
    check_balance!(0, &rpc_client, &sender_pubkey);
    check_balance!(500_000 - fee, &rpc_client, &recipient_pubkey);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_transfer_unfunded_recipient() {
    agave_logger::setup();
    let mint_keypair = Keypair::new();
    let faucet_addr = run_local_faucet_with_unique_port_for_tests(mint_keypair.insecure_clone());
    let test_validator = TestValidator::async_with_custom_fees(
        &mint_keypair,
        1,
        Some(faucet_addr),
        SocketAddrSpace::Unspecified,
    )
    .await;

    let rpc_client =
        RpcClient::new_with_commitment(test_validator.rpc_url(), CommitmentConfig::processed());

    let default_signer = Keypair::new();

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    config.signers = vec![&default_signer];
    config.send_transaction_config.skip_preflight = false;

    let sender_pubkey = config.signers[0].pubkey();
    let recipient_pubkey = Pubkey::from([1u8; 32]);

    request_and_confirm_airdrop(&rpc_client, &config, &sender_pubkey, 50_000)
        .await
        .unwrap();
    check_balance!(50_000, &rpc_client, &sender_pubkey);
    check_balance!(0, &rpc_client, &recipient_pubkey);

    check_ready(&rpc_client).await;

    // Plain ole transfer
    config.command = CliCommand::Transfer {
        amount: SpendAmount::All,
        to: recipient_pubkey,
        from: 0,
        sign_only: false,
        dump_transaction_message: false,
        allow_unfunded_recipient: false,
        no_wait: false,
        blockhash_query: BlockhashQuery::Rpc(Source::Cluster),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        derived_address_seed: None,
        derived_address_program_id: None,
        compute_unit_price: None,
    };

    // Expect failure due to unfunded recipient and the lack of the `allow_unfunded_recipient` flag
    process_command(&config).await.unwrap_err();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_transfer_with_seed() {
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

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    config.signers = vec![&default_signer];

    let sender_pubkey = config.signers[0].pubkey();
    let recipient_pubkey = Pubkey::from([1u8; 32]);
    let derived_address_seed = "seed".to_string();
    let derived_address_program_id = stake::program::id();
    let derived_address = Pubkey::create_with_seed(
        &sender_pubkey,
        &derived_address_seed,
        &derived_address_program_id,
    )
    .unwrap();

    request_and_confirm_airdrop(&rpc_client, &config, &sender_pubkey, LAMPORTS_PER_SOL)
        .await
        .unwrap();
    request_and_confirm_airdrop(&rpc_client, &config, &derived_address, 5 * LAMPORTS_PER_SOL)
        .await
        .unwrap();
    check_balance!(LAMPORTS_PER_SOL, &rpc_client, &sender_pubkey);
    check_balance!(5 * LAMPORTS_PER_SOL, &rpc_client, &derived_address);
    check_balance!(0, &rpc_client, &recipient_pubkey);

    check_ready(&rpc_client).await;

    // Transfer with seed
    config.command = CliCommand::Transfer {
        amount: SpendAmount::Some(5 * LAMPORTS_PER_SOL),
        to: recipient_pubkey,
        from: 0,
        sign_only: false,
        dump_transaction_message: false,
        allow_unfunded_recipient: true,
        no_wait: false,
        blockhash_query: BlockhashQuery::Rpc(Source::Cluster),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        derived_address_seed: Some(derived_address_seed),
        derived_address_program_id: Some(derived_address_program_id),
        compute_unit_price: None,
    };
    process_command(&config).await.unwrap();
    check_balance!(LAMPORTS_PER_SOL - fee, &rpc_client, &sender_pubkey);
    check_balance!(5 * LAMPORTS_PER_SOL, &rpc_client, &recipient_pubkey);
    check_balance!(0, &rpc_client, &derived_address);
}
