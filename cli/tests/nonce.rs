#![allow(clippy::arithmetic_side_effects)]
use {
    solana_cli::{
        check_balance,
        cli::{process_command, request_and_confirm_airdrop, CliCommand, CliConfig},
        spend_utils::SpendAmount,
        test_utils::check_ready,
    },
    solana_cli_output::{parse_sign_only_reply_string, OutputFormat},
    solana_commitment_config::CommitmentConfig,
    solana_faucet::faucet::run_local_faucet_with_unique_port_for_tests,
    solana_hash::Hash,
    solana_keypair::{keypair_from_seed, Keypair},
    solana_native_token::LAMPORTS_PER_SOL,
    solana_net_utils::SocketAddrSpace,
    solana_pubkey::Pubkey,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_rpc_client_nonce_utils::nonblocking::blockhash_query::{BlockhashQuery, Source},
    solana_signer::Signer,
    solana_system_interface::program as system_program,
    solana_test_validator::TestValidator,
    test_case::test_case,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[test_case(None, false, None; "base")]
#[test_case(Some(String::from("seed")), false, None; "with_seed")]
#[test_case(None, true, None; "with_authority")]
#[test_case(None, false, Some(1_000_000); "with_compute_unit_price")]
async fn test_nonce(
    seed: Option<String>,
    use_nonce_authority: bool,
    compute_unit_price: Option<u64>,
) {
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
    let json_rpc_url = test_validator.rpc_url();

    let mut config_payer = CliConfig::recent_for_tests();
    config_payer.json_rpc_url.clone_from(&json_rpc_url);
    let payer = Keypair::new();
    config_payer.signers = vec![&payer];

    request_and_confirm_airdrop(
        &rpc_client,
        &config_payer,
        &config_payer.signers[0].pubkey(),
        2000 * LAMPORTS_PER_SOL,
    )
    .await
    .unwrap();
    check_balance!(
        2000 * LAMPORTS_PER_SOL,
        &rpc_client,
        &config_payer.signers[0].pubkey(),
    );

    let mut config_nonce = CliConfig::recent_for_tests();
    config_nonce.json_rpc_url = json_rpc_url;
    let nonce_keypair = keypair_from_seed(&[0u8; 32]).unwrap();
    config_nonce.signers = vec![&nonce_keypair];

    let nonce_account = if let Some(seed) = seed.as_ref() {
        Pubkey::create_with_seed(
            &config_nonce.signers[0].pubkey(),
            seed,
            &system_program::id(),
        )
        .unwrap()
    } else {
        nonce_keypair.pubkey()
    };

    let nonce_authority = Keypair::new();
    let optional_authority = if use_nonce_authority {
        Some(nonce_authority.pubkey())
    } else {
        None
    };

    // Create nonce account
    config_payer.signers.push(&nonce_keypair);
    config_payer.command = CliCommand::CreateNonceAccount {
        nonce_account: 1,
        seed,
        nonce_authority: optional_authority,
        memo: None,
        amount: SpendAmount::Some(1000 * LAMPORTS_PER_SOL),
        compute_unit_price,
    };

    process_command(&config_payer).await.unwrap();
    check_balance!(
        1000 * LAMPORTS_PER_SOL,
        &rpc_client,
        &config_payer.signers[0].pubkey(),
    );
    check_balance!(1000 * LAMPORTS_PER_SOL, &rpc_client, &nonce_account);

    // Get nonce
    config_payer.signers.pop();
    config_payer.command = CliCommand::GetNonce(nonce_account);
    let first_nonce_string = process_command(&config_payer).await.unwrap();
    let first_nonce = first_nonce_string.parse::<Hash>().unwrap();

    // Get nonce
    config_payer.command = CliCommand::GetNonce(nonce_account);
    let second_nonce_string = process_command(&config_payer).await.unwrap();
    let second_nonce = second_nonce_string.parse::<Hash>().unwrap();

    assert_eq!(first_nonce, second_nonce);

    let mut authorized_signers: Vec<&dyn Signer> = vec![&payer];
    let index = if use_nonce_authority {
        authorized_signers.push(&nonce_authority);
        1
    } else {
        0
    };

    // New nonce
    config_payer.signers.clone_from(&authorized_signers);
    config_payer.command = CliCommand::NewNonce {
        nonce_account,
        nonce_authority: index,
        memo: None,
        compute_unit_price,
    };
    process_command(&config_payer).await.unwrap();

    // Get nonce
    config_payer.signers = vec![&payer];
    config_payer.command = CliCommand::GetNonce(nonce_account);
    let third_nonce_string = process_command(&config_payer).await.unwrap();
    let third_nonce = third_nonce_string.parse::<Hash>().unwrap();

    assert_ne!(first_nonce, third_nonce);

    // Withdraw from nonce account
    let payee_pubkey = solana_pubkey::new_rand();
    config_payer.signers = authorized_signers;
    config_payer.command = CliCommand::WithdrawFromNonceAccount {
        nonce_account,
        nonce_authority: index,
        memo: None,
        destination_account_pubkey: payee_pubkey,
        lamports: 100 * LAMPORTS_PER_SOL,
        compute_unit_price,
    };
    process_command(&config_payer).await.unwrap();
    check_balance!(
        1000 * LAMPORTS_PER_SOL,
        &rpc_client,
        &config_payer.signers[0].pubkey(),
    );
    check_balance!(900 * LAMPORTS_PER_SOL, &rpc_client, &nonce_account);
    check_balance!(100 * LAMPORTS_PER_SOL, &rpc_client, &payee_pubkey);

    // Show nonce account
    config_payer.command = CliCommand::ShowNonceAccount {
        nonce_account_pubkey: nonce_account,
        use_lamports_unit: true,
    };
    process_command(&config_payer).await.unwrap();

    // Set new authority
    let new_authority = Keypair::new();
    config_payer.command = CliCommand::AuthorizeNonceAccount {
        nonce_account,
        nonce_authority: index,
        memo: None,
        new_authority: new_authority.pubkey(),
        compute_unit_price,
    };
    process_command(&config_payer).await.unwrap();

    // Old authority fails now
    config_payer.command = CliCommand::NewNonce {
        nonce_account,
        nonce_authority: index,
        memo: None,
        compute_unit_price,
    };
    process_command(&config_payer).await.unwrap_err();

    // New authority can advance nonce
    config_payer.signers = vec![&payer, &new_authority];
    config_payer.command = CliCommand::NewNonce {
        nonce_account,
        nonce_authority: 1,
        memo: None,
        compute_unit_price,
    };
    process_command(&config_payer).await.unwrap();

    // New authority can withdraw from nonce account
    config_payer.command = CliCommand::WithdrawFromNonceAccount {
        nonce_account,
        nonce_authority: 1,
        memo: None,
        destination_account_pubkey: payee_pubkey,
        lamports: 100 * LAMPORTS_PER_SOL,
        compute_unit_price,
    };
    process_command(&config_payer).await.unwrap();
    check_balance!(
        1000 * LAMPORTS_PER_SOL,
        &rpc_client,
        &config_payer.signers[0].pubkey(),
    );
    check_balance!(800 * LAMPORTS_PER_SOL, &rpc_client, &nonce_account);
    check_balance!(200 * LAMPORTS_PER_SOL, &rpc_client, &payee_pubkey);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_create_account_with_seed() {
    const ONE_SIG_FEE: u64 = 5000;
    agave_logger::setup();
    let mint_keypair = Keypair::new();
    let faucet_addr = run_local_faucet_with_unique_port_for_tests(mint_keypair.insecure_clone());
    let test_validator = TestValidator::async_with_custom_fees(
        &mint_keypair,
        ONE_SIG_FEE,
        Some(faucet_addr),
        SocketAddrSpace::Unspecified,
    )
    .await;

    let offline_nonce_authority_signer = keypair_from_seed(&[1u8; 32]).unwrap();
    let online_nonce_creator_signer = keypair_from_seed(&[2u8; 32]).unwrap();
    let to_address = Pubkey::from([3u8; 32]);

    // Setup accounts
    let rpc_client =
        RpcClient::new_with_commitment(test_validator.rpc_url(), CommitmentConfig::processed());
    request_and_confirm_airdrop(
        &rpc_client,
        &CliConfig::recent_for_tests(),
        &offline_nonce_authority_signer.pubkey(),
        42 * LAMPORTS_PER_SOL,
    )
    .await
    .unwrap();
    request_and_confirm_airdrop(
        &rpc_client,
        &CliConfig::recent_for_tests(),
        &online_nonce_creator_signer.pubkey(),
        4242 * LAMPORTS_PER_SOL,
    )
    .await
    .unwrap();
    check_balance!(
        42 * LAMPORTS_PER_SOL,
        &rpc_client,
        &offline_nonce_authority_signer.pubkey(),
    );
    check_balance!(
        4242 * LAMPORTS_PER_SOL,
        &rpc_client,
        &online_nonce_creator_signer.pubkey(),
    );
    check_balance!(0, &rpc_client, &to_address);

    check_ready(&rpc_client).await;

    // Create nonce account
    let creator_pubkey = online_nonce_creator_signer.pubkey();
    let authority_pubkey = offline_nonce_authority_signer.pubkey();
    let seed = authority_pubkey.to_string()[0..32].to_string();
    let nonce_address =
        Pubkey::create_with_seed(&creator_pubkey, &seed, &system_program::id()).unwrap();
    check_balance!(0, &rpc_client, &nonce_address);

    let mut creator_config = CliConfig::recent_for_tests();
    creator_config.json_rpc_url = test_validator.rpc_url();
    creator_config.signers = vec![&online_nonce_creator_signer];
    creator_config.command = CliCommand::CreateNonceAccount {
        nonce_account: 0,
        seed: Some(seed),
        nonce_authority: Some(authority_pubkey),
        memo: None,
        amount: SpendAmount::Some(241 * LAMPORTS_PER_SOL),
        compute_unit_price: None,
    };
    process_command(&creator_config).await.unwrap();
    check_balance!(241 * LAMPORTS_PER_SOL, &rpc_client, &nonce_address);
    check_balance!(
        42 * LAMPORTS_PER_SOL,
        &rpc_client,
        &offline_nonce_authority_signer.pubkey(),
    );
    check_balance!(
        4001 * LAMPORTS_PER_SOL - ONE_SIG_FEE,
        &rpc_client,
        &online_nonce_creator_signer.pubkey(),
    );
    check_balance!(0, &rpc_client, &to_address);

    // Fetch nonce hash
    let nonce_hash = solana_rpc_client_nonce_utils::nonblocking::get_account_with_commitment(
        &rpc_client,
        &nonce_address,
        CommitmentConfig::processed(),
    )
    .await
    .and_then(|ref a| solana_rpc_client_nonce_utils::data_from_account(a))
    .unwrap()
    .blockhash();

    // Test by creating transfer TX with nonce, fully offline
    let mut authority_config = CliConfig::recent_for_tests();
    authority_config.json_rpc_url = String::default();
    authority_config.signers = vec![&offline_nonce_authority_signer];
    // Verify we cannot contact the cluster
    authority_config.command = CliCommand::ClusterVersion;
    process_command(&authority_config).await.unwrap_err();
    authority_config.command = CliCommand::Transfer {
        amount: SpendAmount::Some(10 * LAMPORTS_PER_SOL),
        to: to_address,
        from: 0,
        sign_only: true,
        dump_transaction_message: true,
        allow_unfunded_recipient: true,
        no_wait: false,
        blockhash_query: BlockhashQuery::Static(nonce_hash),
        nonce_account: Some(nonce_address),
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        derived_address_seed: None,
        derived_address_program_id: None,
        compute_unit_price: None,
    };
    authority_config.output_format = OutputFormat::JsonCompact;
    let sign_only_reply = process_command(&authority_config).await.unwrap();
    let sign_only = parse_sign_only_reply_string(&sign_only_reply);
    let authority_presigner = sign_only.presigner_of(&authority_pubkey).unwrap();
    assert_eq!(sign_only.blockhash, nonce_hash);

    // And submit it
    let mut submit_config = CliConfig::recent_for_tests();
    submit_config.json_rpc_url = test_validator.rpc_url();
    submit_config.signers = vec![&authority_presigner];
    submit_config.command = CliCommand::Transfer {
        amount: SpendAmount::Some(10 * LAMPORTS_PER_SOL),
        to: to_address,
        from: 0,
        sign_only: false,
        dump_transaction_message: true,
        allow_unfunded_recipient: true,
        no_wait: false,
        blockhash_query: BlockhashQuery::Validated(
            Source::NonceAccount(nonce_address),
            sign_only.blockhash,
        ),
        nonce_account: Some(nonce_address),
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        derived_address_seed: None,
        derived_address_program_id: None,
        compute_unit_price: None,
    };
    process_command(&submit_config).await.unwrap();
    check_balance!(241 * LAMPORTS_PER_SOL, &rpc_client, &nonce_address);
    check_balance!(
        32 * LAMPORTS_PER_SOL - ONE_SIG_FEE,
        &rpc_client,
        &offline_nonce_authority_signer.pubkey(),
    );
    check_balance!(
        4001 * LAMPORTS_PER_SOL - ONE_SIG_FEE,
        &rpc_client,
        &online_nonce_creator_signer.pubkey(),
    );
    check_balance!(10 * LAMPORTS_PER_SOL, &rpc_client, &to_address);
}
