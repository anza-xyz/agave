#![allow(clippy::arithmetic_side_effects)]
use {
    assert_matches::assert_matches,
    solana_account::state_traits::StateMut,
    solana_cli::{
        check_balance,
        cli::{process_command, request_and_confirm_airdrop, CliCommand, CliConfig},
        spend_utils::SpendAmount,
        stake::StakeAuthorizationIndexed,
        test_utils::{check_ready, wait_for_next_epoch_plus_n_slots},
    },
    solana_cli_output::{parse_sign_only_reply_string, OutputFormat},
    solana_client::nonblocking::blockhash_query::{BlockhashQuery, Source},
    solana_commitment_config::CommitmentConfig,
    solana_epoch_schedule::EpochSchedule,
    solana_faucet::faucet::run_local_faucet_with_unique_port_for_tests,
    solana_fee_calculator::FeeRateGovernor,
    solana_fee_structure::FeeStructure,
    solana_keypair::{keypair_from_seed, Keypair},
    solana_native_token::LAMPORTS_PER_SOL,
    solana_net_utils::SocketAddrSpace,
    solana_nonce::state::State as NonceState,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_rpc_client_api::request::DELINQUENT_VALIDATOR_SLOT_DISTANCE,
    solana_signer::Signer,
    solana_stake_interface::{
        self as stake,
        instruction::LockupArgs,
        state::{Lockup, StakeAuthorize, StakeStateV2},
    },
    solana_test_validator::{TestValidator, TestValidatorGenesis},
    test_case::test_case,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_stake_delegation_force() {
    let mint_keypair = Keypair::new();
    let authorized_withdrawer = Keypair::new().pubkey();
    let faucet_addr = run_local_faucet_with_unique_port_for_tests(mint_keypair.insecure_clone());
    let slots_per_epoch = 32;
    let test_validator = TestValidatorGenesis::default()
        .fee_rate_governor(FeeRateGovernor::new(0, 0))
        .rent(Rent {
            lamports_per_byte_year: 1,
            exemption_threshold: 1.0,
            ..Rent::default()
        })
        .epoch_schedule(EpochSchedule::custom(
            slots_per_epoch,
            slots_per_epoch,
            /* enable_warmup_epochs = */ false,
        ))
        .faucet_addr(Some(faucet_addr))
        .warp_slot(DELINQUENT_VALIDATOR_SLOT_DISTANCE * 2) // get out in front of the cli voter delinquency check
        .start_async_with_mint_address(&mint_keypair, SocketAddrSpace::Unspecified)
        .await
        .expect("validator start failed");

    let rpc_client =
        RpcClient::new_with_commitment(test_validator.rpc_url(), CommitmentConfig::processed());
    let default_signer = Keypair::new();

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    config.signers = vec![&default_signer];

    request_and_confirm_airdrop(
        &rpc_client,
        &config,
        &config.signers[0].pubkey(),
        100_000_000_000,
    )
    .await
    .unwrap();

    // Create vote account
    let vote_keypair = Keypair::new();
    config.signers = vec![&default_signer, &vote_keypair];
    config.command = CliCommand::CreateVoteAccount {
        vote_account: 1,
        seed: None,
        identity_account: 0,
        authorized_voter: None,
        authorized_withdrawer,
        commission: 0,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Rpc(Source::Cluster),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        compute_unit_price: None,
    };
    process_command(&config).await.unwrap();

    // Create stake account
    let stake_keypair = Keypair::new();
    config.signers = vec![&default_signer, &stake_keypair];
    config.command = CliCommand::CreateStakeAccount {
        stake_account: 1,
        seed: None,
        staker: None,
        withdrawer: None,
        withdrawer_signer: None,
        lockup: Lockup::default(),
        amount: SpendAmount::Some(25_000_000_000),
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Rpc(Source::Cluster),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        from: 0,
        compute_unit_price: None,
    };
    process_command(&config).await.unwrap();

    // Delegate stake succeeds despite no votes, because voter has zero stake
    config.signers = vec![&default_signer];
    config.command = CliCommand::DelegateStake {
        stake_account_pubkey: stake_keypair.pubkey(),
        vote_account_pubkey: vote_keypair.pubkey(),
        stake_authority: 0,
        force: false,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::default(),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        compute_unit_price: None,
    };
    process_command(&config).await.unwrap();

    // Create a second stake account
    let stake_keypair2 = Keypair::new();
    config.signers = vec![&default_signer, &stake_keypair2];
    config.command = CliCommand::CreateStakeAccount {
        stake_account: 1,
        seed: None,
        staker: None,
        withdrawer: None,
        withdrawer_signer: None,
        lockup: Lockup::default(),
        amount: SpendAmount::Some(25_000_000_000),
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Rpc(Source::Cluster),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        from: 0,
        compute_unit_price: None,
    };
    process_command(&config).await.unwrap();

    wait_for_next_epoch_plus_n_slots(&rpc_client, 1).await;

    // Delegate stake2 fails because voter has not voted, but is now staked
    config.signers = vec![&default_signer];
    config.command = CliCommand::DelegateStake {
        stake_account_pubkey: stake_keypair2.pubkey(),
        vote_account_pubkey: vote_keypair.pubkey(),
        stake_authority: 0,
        force: false,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::default(),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        compute_unit_price: None,
    };
    process_command(&config).await.unwrap_err();

    // But if we force it, it works anyway!
    config.command = CliCommand::DelegateStake {
        stake_account_pubkey: stake_keypair2.pubkey(),
        vote_account_pubkey: vote_keypair.pubkey(),
        stake_authority: 0,
        force: true,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::default(),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        compute_unit_price: None,
    };
    process_command(&config).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[test_case(None; "base")]
#[test_case(Some(1_000_000); "with_compute_unit_price")]
async fn test_seed_stake_delegation_and_deactivation(compute_unit_price: Option<u64>) {
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

    let stake_address = Pubkey::create_with_seed(
        &config_validator.signers[0].pubkey(),
        "hi there",
        &stake::program::id(),
    )
    .expect("bad seed");

    // Create stake account with a seed, uses the validator config as the base,
    //   which is nice ;)
    config_validator.command = CliCommand::CreateStakeAccount {
        stake_account: 0,
        seed: Some("hi there".to_string()),
        staker: None,
        withdrawer: None,
        withdrawer_signer: None,
        lockup: Lockup::default(),
        amount: SpendAmount::Some(50_000_000_000),
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Rpc(Source::Cluster),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        from: 0,
        compute_unit_price,
    };
    process_command(&config_validator).await.unwrap();

    // Delegate stake
    config_validator.command = CliCommand::DelegateStake {
        stake_account_pubkey: stake_address,
        vote_account_pubkey: test_validator.vote_account_address(),
        stake_authority: 0,
        force: true,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::default(),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        compute_unit_price,
    };
    process_command(&config_validator).await.unwrap();

    // Deactivate stake
    config_validator.command = CliCommand::DeactivateStake {
        stake_account_pubkey: stake_address,
        stake_authority: 0,
        sign_only: false,
        deactivate_delinquent: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::default(),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        seed: None,
        fee_payer: 0,
        compute_unit_price,
    };
    process_command(&config_validator).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_stake_delegation_and_withdraw_available() {
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
    let validator_keypair = Keypair::new();

    let mut config_validator = CliConfig::recent_for_tests();
    config_validator.json_rpc_url = test_validator.rpc_url();
    config_validator.signers = vec![&validator_keypair];

    let stake_keypair = keypair_from_seed(&[0u8; 32]).unwrap();
    let recipient_pubkey = Pubkey::new_unique();

    request_and_confirm_airdrop(
        &rpc_client,
        &config_validator,
        &config_validator.signers[0].pubkey(),
        100 * LAMPORTS_PER_SOL,
    )
    .await
    .unwrap();
    check_balance!(
        100_000_000_000,
        &rpc_client,
        &config_validator.signers[0].pubkey()
    );

    // Create stake account
    config_validator.signers.push(&stake_keypair);
    config_validator.command = CliCommand::CreateStakeAccount {
        stake_account: 1,
        seed: None,
        staker: None,
        withdrawer: None,
        withdrawer_signer: None,
        lockup: Lockup::default(),
        amount: SpendAmount::Some(50 * LAMPORTS_PER_SOL),
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Rpc(Source::Cluster),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        from: 0,
        compute_unit_price: None,
    };
    process_command(&config_validator).await.unwrap();

    // Delegate stake
    config_validator.signers.pop();
    config_validator.command = CliCommand::DelegateStake {
        stake_account_pubkey: stake_keypair.pubkey(),
        vote_account_pubkey: test_validator.vote_account_address(),
        stake_authority: 0,
        force: true,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::default(),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        compute_unit_price: None,
    };
    process_command(&config_validator).await.unwrap();

    // Withdraw available stake
    config_validator.signers = vec![&validator_keypair];
    config_validator.command = CliCommand::WithdrawStake {
        stake_account_pubkey: stake_keypair.pubkey(),
        destination_account_pubkey: recipient_pubkey,
        amount: SpendAmount::Available,
        withdraw_authority: 0,
        custodian: None,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Rpc(Source::Cluster),
        nonce_authority: 0,
        nonce_account: None,
        memo: None,
        seed: None,
        fee_payer: 0,
        compute_unit_price: None,
    };
    process_command(&config_validator).await.unwrap();
    // While withdraw transaction succeeds, no lamports move because all stake
    // is activating
    check_balance!(0, &rpc_client, &recipient_pubkey);

    // Add extra SOL to the stake account
    request_and_confirm_airdrop(
        &rpc_client,
        &config_validator,
        &stake_keypair.pubkey(),
        5 * LAMPORTS_PER_SOL,
    )
    .await
    .unwrap();
    check_balance!(55 * LAMPORTS_PER_SOL, &rpc_client, &stake_keypair.pubkey());

    // Withdraw available stake
    config_validator.signers = vec![&validator_keypair];
    config_validator.command = CliCommand::WithdrawStake {
        stake_account_pubkey: stake_keypair.pubkey(),
        destination_account_pubkey: recipient_pubkey,
        amount: SpendAmount::Available,
        withdraw_authority: 0,
        custodian: None,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Rpc(Source::Cluster),
        nonce_authority: 0,
        nonce_account: None,
        memo: None,
        seed: None,
        fee_payer: 0,
        compute_unit_price: None,
    };
    process_command(&config_validator).await.unwrap();
    // Extra (inactive) SOL is withdrawn
    check_balance!(5 * LAMPORTS_PER_SOL, &rpc_client, &recipient_pubkey);

    // Deactivate stake
    config_validator.command = CliCommand::DeactivateStake {
        stake_account_pubkey: stake_keypair.pubkey(),
        stake_authority: 0,
        sign_only: false,
        deactivate_delinquent: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::default(),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        seed: None,
        fee_payer: 0,
        compute_unit_price: None,
    };
    process_command(&config_validator).await.unwrap();

    // Withdraw available stake
    config_validator.signers = vec![&validator_keypair];
    config_validator.command = CliCommand::WithdrawStake {
        stake_account_pubkey: stake_keypair.pubkey(),
        destination_account_pubkey: recipient_pubkey,
        amount: SpendAmount::Available,
        withdraw_authority: 0,
        custodian: None,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Rpc(Source::Cluster),
        nonce_authority: 0,
        nonce_account: None,
        memo: None,
        seed: None,
        fee_payer: 0,
        compute_unit_price: None,
    };
    process_command(&config_validator).await.unwrap();
    // Complete balance is withdrawn because all stake is inactive
    check_balance!(55 * LAMPORTS_PER_SOL, &rpc_client, &recipient_pubkey);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_stake_delegation_and_withdraw_all() {
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
    let validator_keypair = Keypair::new();

    let mut config_validator = CliConfig::recent_for_tests();
    config_validator.json_rpc_url = test_validator.rpc_url();
    config_validator.signers = vec![&validator_keypair];

    let stake_keypair = keypair_from_seed(&[0u8; 32]).unwrap();
    let recipient_pubkey = Pubkey::new_unique();

    request_and_confirm_airdrop(
        &rpc_client,
        &config_validator,
        &config_validator.signers[0].pubkey(),
        100 * LAMPORTS_PER_SOL,
    )
    .await
    .unwrap();
    check_balance!(
        100_000_000_000,
        &rpc_client,
        &config_validator.signers[0].pubkey()
    );

    // Create stake account
    config_validator.signers.push(&stake_keypair);
    config_validator.command = CliCommand::CreateStakeAccount {
        stake_account: 1,
        seed: None,
        staker: None,
        withdrawer: None,
        withdrawer_signer: None,
        lockup: Lockup::default(),
        amount: SpendAmount::Some(50 * LAMPORTS_PER_SOL),
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Rpc(Source::Cluster),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        from: 0,
        compute_unit_price: None,
    };
    process_command(&config_validator).await.unwrap();

    // Delegate stake
    config_validator.signers.pop();
    config_validator.command = CliCommand::DelegateStake {
        stake_account_pubkey: stake_keypair.pubkey(),
        vote_account_pubkey: test_validator.vote_account_address(),
        stake_authority: 0,
        force: true,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::default(),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        compute_unit_price: None,
    };
    process_command(&config_validator).await.unwrap();

    // Withdraw all stake fails because stake is activating
    config_validator.signers = vec![&validator_keypair];
    config_validator.command = CliCommand::WithdrawStake {
        stake_account_pubkey: stake_keypair.pubkey(),
        destination_account_pubkey: recipient_pubkey,
        amount: SpendAmount::All,
        withdraw_authority: 0,
        custodian: None,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Rpc(Source::Cluster),
        nonce_authority: 0,
        nonce_account: None,
        memo: None,
        seed: None,
        fee_payer: 0,
        compute_unit_price: None,
    };
    process_command(&config_validator).await.unwrap_err();

    // Add extra SOL to the stake account
    request_and_confirm_airdrop(
        &rpc_client,
        &config_validator,
        &stake_keypair.pubkey(),
        5 * LAMPORTS_PER_SOL,
    )
    .await
    .unwrap();
    check_balance!(55 * LAMPORTS_PER_SOL, &rpc_client, &stake_keypair.pubkey());

    // Withdraw all stake still fails, because it attempts to withdraw both
    // activating and inactive stake
    config_validator.signers = vec![&validator_keypair];
    config_validator.command = CliCommand::WithdrawStake {
        stake_account_pubkey: stake_keypair.pubkey(),
        destination_account_pubkey: recipient_pubkey,
        amount: SpendAmount::All,
        withdraw_authority: 0,
        custodian: None,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Rpc(Source::Cluster),
        nonce_authority: 0,
        nonce_account: None,
        memo: None,
        seed: None,
        fee_payer: 0,
        compute_unit_price: None,
    };
    process_command(&config_validator).await.unwrap_err();

    // Deactivate stake
    config_validator.command = CliCommand::DeactivateStake {
        stake_account_pubkey: stake_keypair.pubkey(),
        stake_authority: 0,
        sign_only: false,
        deactivate_delinquent: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::default(),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        seed: None,
        fee_payer: 0,
        compute_unit_price: None,
    };
    process_command(&config_validator).await.unwrap();

    // Withdraw stake
    config_validator.signers = vec![&validator_keypair];
    config_validator.command = CliCommand::WithdrawStake {
        stake_account_pubkey: stake_keypair.pubkey(),
        destination_account_pubkey: recipient_pubkey,
        amount: SpendAmount::All,
        withdraw_authority: 0,
        custodian: None,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Rpc(Source::Cluster),
        nonce_authority: 0,
        nonce_account: None,
        memo: None,
        seed: None,
        fee_payer: 0,
        compute_unit_price: None,
    };
    process_command(&config_validator).await.unwrap();
    check_balance!(55 * LAMPORTS_PER_SOL, &rpc_client, &recipient_pubkey);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[test_case(None; "base")]
#[test_case(Some(1_000_000); "with_compute_unit_price")]
async fn test_stake_delegation_and_deactivation(compute_unit_price: Option<u64>) {
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
    let validator_keypair = Keypair::new();

    let mut config_validator = CliConfig::recent_for_tests();
    config_validator.json_rpc_url = test_validator.rpc_url();
    config_validator.signers = vec![&validator_keypair];

    let stake_keypair = keypair_from_seed(&[0u8; 32]).unwrap();

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

    // Create stake account
    config_validator.signers.push(&stake_keypair);
    config_validator.command = CliCommand::CreateStakeAccount {
        stake_account: 1,
        seed: None,
        staker: None,
        withdrawer: None,
        withdrawer_signer: None,
        lockup: Lockup::default(),
        amount: SpendAmount::Some(50_000_000_000),
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Rpc(Source::Cluster),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        from: 0,
        compute_unit_price,
    };
    process_command(&config_validator).await.unwrap();

    // Delegate stake
    config_validator.signers.pop();
    config_validator.command = CliCommand::DelegateStake {
        stake_account_pubkey: stake_keypair.pubkey(),
        vote_account_pubkey: test_validator.vote_account_address(),
        stake_authority: 0,
        force: true,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::default(),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        compute_unit_price,
    };
    process_command(&config_validator).await.unwrap();

    // Deactivate stake
    config_validator.command = CliCommand::DeactivateStake {
        stake_account_pubkey: stake_keypair.pubkey(),
        stake_authority: 0,
        sign_only: false,
        deactivate_delinquent: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::default(),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        seed: None,
        fee_payer: 0,
        compute_unit_price,
    };
    process_command(&config_validator).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[test_case(None; "base")]
#[test_case(Some(1_000_000); "with_compute_unit_price")]
async fn test_offline_stake_delegation_and_deactivation(compute_unit_price: Option<u64>) {
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

    let mut config_validator = CliConfig::recent_for_tests();
    config_validator.json_rpc_url = test_validator.rpc_url();
    let validator_keypair = Keypair::new();
    config_validator.signers = vec![&validator_keypair];

    let mut config_payer = CliConfig::recent_for_tests();
    config_payer.json_rpc_url = test_validator.rpc_url();

    let stake_keypair = keypair_from_seed(&[0u8; 32]).unwrap();

    let mut config_offline = CliConfig::recent_for_tests();
    config_offline.json_rpc_url = String::default();
    config_offline.command = CliCommand::ClusterVersion;
    let offline_keypair = Keypair::new();
    config_offline.signers = vec![&offline_keypair];
    // Verify that we cannot reach the cluster
    process_command(&config_offline).await.unwrap_err();

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

    request_and_confirm_airdrop(
        &rpc_client,
        &config_offline,
        &config_offline.signers[0].pubkey(),
        100_000_000_000,
    )
    .await
    .unwrap();
    check_balance!(
        100_000_000_000,
        &rpc_client,
        &config_offline.signers[0].pubkey()
    );

    // Create stake account
    config_validator.signers.push(&stake_keypair);
    config_validator.command = CliCommand::CreateStakeAccount {
        stake_account: 1,
        seed: None,
        staker: Some(config_offline.signers[0].pubkey()),
        withdrawer: None,
        withdrawer_signer: None,
        lockup: Lockup::default(),
        amount: SpendAmount::Some(50_000_000_000),
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Rpc(Source::Cluster),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        from: 0,
        compute_unit_price,
    };
    process_command(&config_validator).await.unwrap();

    // Delegate stake offline
    let blockhash = rpc_client.get_latest_blockhash().await.unwrap();
    config_offline.command = CliCommand::DelegateStake {
        stake_account_pubkey: stake_keypair.pubkey(),
        vote_account_pubkey: test_validator.vote_account_address(),
        stake_authority: 0,
        force: true,
        sign_only: true,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Static(blockhash),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        compute_unit_price,
    };
    config_offline.output_format = OutputFormat::JsonCompact;
    let sig_response = process_command(&config_offline).await.unwrap();
    let sign_only = parse_sign_only_reply_string(&sig_response);
    assert!(sign_only.has_all_signers());
    let offline_presigner = sign_only
        .presigner_of(&config_offline.signers[0].pubkey())
        .unwrap();
    config_payer.signers = vec![&offline_presigner];
    config_payer.command = CliCommand::DelegateStake {
        stake_account_pubkey: stake_keypair.pubkey(),
        vote_account_pubkey: test_validator.vote_account_address(),
        stake_authority: 0,
        force: true,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Validated(Source::Cluster, blockhash),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        compute_unit_price,
    };
    process_command(&config_payer).await.unwrap();

    // Deactivate stake offline
    let blockhash = rpc_client.get_latest_blockhash().await.unwrap();
    config_offline.command = CliCommand::DeactivateStake {
        stake_account_pubkey: stake_keypair.pubkey(),
        stake_authority: 0,
        sign_only: true,
        deactivate_delinquent: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Static(blockhash),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        seed: None,
        fee_payer: 0,
        compute_unit_price,
    };
    let sig_response = process_command(&config_offline).await.unwrap();
    let sign_only = parse_sign_only_reply_string(&sig_response);
    assert!(sign_only.has_all_signers());
    let offline_presigner = sign_only
        .presigner_of(&config_offline.signers[0].pubkey())
        .unwrap();
    config_payer.signers = vec![&offline_presigner];
    config_payer.command = CliCommand::DeactivateStake {
        stake_account_pubkey: stake_keypair.pubkey(),
        stake_authority: 0,
        sign_only: false,
        deactivate_delinquent: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Validated(Source::Cluster, blockhash),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        seed: None,
        fee_payer: 0,
        compute_unit_price,
    };
    process_command(&config_payer).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[test_case(None; "base")]
#[test_case(Some(1_000_000); "with_compute_unit_price")]
async fn test_nonced_stake_delegation_and_deactivation(compute_unit_price: Option<u64>) {
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

    let config_keypair = keypair_from_seed(&[0u8; 32]).unwrap();
    let mut config = CliConfig::recent_for_tests();
    config.signers = vec![&config_keypair];
    config.json_rpc_url = test_validator.rpc_url();

    let minimum_nonce_balance = rpc_client
        .get_minimum_balance_for_rent_exemption(NonceState::size())
        .await
        .unwrap();

    request_and_confirm_airdrop(
        &rpc_client,
        &config,
        &config.signers[0].pubkey(),
        100_000_000_000,
    )
    .await
    .unwrap();

    // Create stake account
    let stake_keypair = Keypair::new();
    config.signers.push(&stake_keypair);
    config.command = CliCommand::CreateStakeAccount {
        stake_account: 1,
        seed: None,
        staker: None,
        withdrawer: None,
        withdrawer_signer: None,
        lockup: Lockup::default(),
        amount: SpendAmount::Some(50_000_000_000),
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Rpc(Source::Cluster),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        from: 0,
        compute_unit_price,
    };
    process_command(&config).await.unwrap();

    // Create nonce account
    let nonce_account = Keypair::new();
    config.signers[1] = &nonce_account;
    config.command = CliCommand::CreateNonceAccount {
        nonce_account: 1,
        seed: None,
        nonce_authority: Some(config.signers[0].pubkey()),
        memo: None,
        amount: SpendAmount::Some(minimum_nonce_balance),
        compute_unit_price,
    };
    process_command(&config).await.unwrap();

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

    // Delegate stake
    config.signers = vec![&config_keypair];
    config.command = CliCommand::DelegateStake {
        stake_account_pubkey: stake_keypair.pubkey(),
        vote_account_pubkey: test_validator.vote_account_address(),
        stake_authority: 0,
        force: true,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Validated(
            Source::NonceAccount(nonce_account.pubkey()),
            nonce_hash,
        ),
        nonce_account: Some(nonce_account.pubkey()),
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        compute_unit_price,
    };
    process_command(&config).await.unwrap();

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

    // Deactivate stake
    config.command = CliCommand::DeactivateStake {
        stake_account_pubkey: stake_keypair.pubkey(),
        stake_authority: 0,
        sign_only: false,
        deactivate_delinquent: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Validated(
            Source::NonceAccount(nonce_account.pubkey()),
            nonce_hash,
        ),
        nonce_account: Some(nonce_account.pubkey()),
        nonce_authority: 0,
        memo: None,
        seed: None,
        fee_payer: 0,
        compute_unit_price,
    };
    process_command(&config).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[test_case(None; "base")]
#[test_case(Some(1_000_000); "with_compute_unit_price")]
async fn test_stake_authorize(compute_unit_price: Option<u64>) {
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
    let default_signer = Keypair::new();

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    config.signers = vec![&default_signer];

    request_and_confirm_airdrop(
        &rpc_client,
        &config,
        &config.signers[0].pubkey(),
        100_000_000_000,
    )
    .await
    .unwrap();

    let offline_keypair = keypair_from_seed(&[0u8; 32]).unwrap();
    let mut config_offline = CliConfig::recent_for_tests();
    config_offline.signers = vec![&offline_keypair];
    config_offline.json_rpc_url = String::default();
    let offline_authority_pubkey = config_offline.signers[0].pubkey();
    config_offline.command = CliCommand::ClusterVersion;
    // Verify that we cannot reach the cluster
    process_command(&config_offline).await.unwrap_err();

    request_and_confirm_airdrop(
        &rpc_client,
        &config_offline,
        &config_offline.signers[0].pubkey(),
        100_000_000_000,
    )
    .await
    .unwrap();

    // Create stake account, identity is authority
    let stake_keypair = Keypair::new();
    let stake_account_pubkey = stake_keypair.pubkey();
    config.signers.push(&stake_keypair);
    config.command = CliCommand::CreateStakeAccount {
        stake_account: 1,
        seed: None,
        staker: None,
        withdrawer: None,
        withdrawer_signer: None,
        lockup: Lockup::default(),
        amount: SpendAmount::Some(50_000_000_000),
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Rpc(Source::Cluster),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        from: 0,
        compute_unit_price,
    };
    process_command(&config).await.unwrap();

    // Assign new online stake authority
    let online_authority = Keypair::new();
    let online_authority_pubkey = online_authority.pubkey();
    config.signers.pop();
    config.command = CliCommand::StakeAuthorize {
        stake_account_pubkey,
        new_authorizations: vec![StakeAuthorizationIndexed {
            authorization_type: StakeAuthorize::Staker,
            new_authority_pubkey: online_authority_pubkey,
            authority: 0,
            new_authority_signer: None,
        }],
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::default(),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        custodian: None,
        no_wait: false,
        compute_unit_price,
    };
    process_command(&config).await.unwrap();
    let stake_account = rpc_client.get_account(&stake_account_pubkey).await.unwrap();
    let stake_state: StakeStateV2 = stake_account.state().unwrap();
    let current_authority = match stake_state {
        StakeStateV2::Initialized(meta) => meta.authorized.staker,
        _ => panic!("Unexpected stake state!"),
    };
    assert_eq!(current_authority, online_authority_pubkey);

    // Assign new online stake and withdraw authorities
    let online_authority2 = Keypair::new();
    let online_authority2_pubkey = online_authority2.pubkey();
    let withdraw_authority = Keypair::new();
    let withdraw_authority_pubkey = withdraw_authority.pubkey();
    config.signers.push(&online_authority);
    config.command = CliCommand::StakeAuthorize {
        stake_account_pubkey,
        new_authorizations: vec![
            StakeAuthorizationIndexed {
                authorization_type: StakeAuthorize::Staker,
                new_authority_pubkey: online_authority2_pubkey,
                authority: 1,
                new_authority_signer: None,
            },
            StakeAuthorizationIndexed {
                authorization_type: StakeAuthorize::Withdrawer,
                new_authority_pubkey: withdraw_authority_pubkey,
                authority: 0,
                new_authority_signer: None,
            },
        ],
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::default(),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        custodian: None,
        no_wait: false,
        compute_unit_price,
    };
    process_command(&config).await.unwrap();
    let stake_account = rpc_client.get_account(&stake_account_pubkey).await.unwrap();
    let stake_state: StakeStateV2 = stake_account.state().unwrap();
    let (current_staker, current_withdrawer) = match stake_state {
        StakeStateV2::Initialized(meta) => (meta.authorized.staker, meta.authorized.withdrawer),
        _ => panic!("Unexpected stake state!"),
    };
    assert_eq!(current_staker, online_authority2_pubkey);
    assert_eq!(current_withdrawer, withdraw_authority_pubkey);

    // Assign new offline stake authority
    config.signers.pop();
    config.signers.push(&online_authority2);
    config.command = CliCommand::StakeAuthorize {
        stake_account_pubkey,
        new_authorizations: vec![StakeAuthorizationIndexed {
            authorization_type: StakeAuthorize::Staker,
            new_authority_pubkey: offline_authority_pubkey,
            authority: 1,
            new_authority_signer: None,
        }],
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::default(),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        custodian: None,
        no_wait: false,
        compute_unit_price,
    };
    process_command(&config).await.unwrap();
    let stake_account = rpc_client.get_account(&stake_account_pubkey).await.unwrap();
    let stake_state: StakeStateV2 = stake_account.state().unwrap();
    let current_authority = match stake_state {
        StakeStateV2::Initialized(meta) => meta.authorized.staker,
        _ => panic!("Unexpected stake state!"),
    };
    assert_eq!(current_authority, offline_authority_pubkey);

    // Offline assignment of new nonced stake authority
    let nonced_authority = Keypair::new();
    let nonced_authority_pubkey = nonced_authority.pubkey();
    let blockhash = rpc_client.get_latest_blockhash().await.unwrap();
    config_offline.command = CliCommand::StakeAuthorize {
        stake_account_pubkey,
        new_authorizations: vec![StakeAuthorizationIndexed {
            authorization_type: StakeAuthorize::Staker,
            new_authority_pubkey: nonced_authority_pubkey,
            authority: 0,
            new_authority_signer: None,
        }],
        sign_only: true,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Static(blockhash),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        custodian: None,
        no_wait: false,
        compute_unit_price,
    };
    config_offline.output_format = OutputFormat::JsonCompact;
    let sign_reply = process_command(&config_offline).await.unwrap();
    let sign_only = parse_sign_only_reply_string(&sign_reply);
    assert!(sign_only.has_all_signers());
    let offline_presigner = sign_only.presigner_of(&offline_authority_pubkey).unwrap();
    config.signers = vec![&offline_presigner];
    config.command = CliCommand::StakeAuthorize {
        stake_account_pubkey,
        new_authorizations: vec![StakeAuthorizationIndexed {
            authorization_type: StakeAuthorize::Staker,
            new_authority_pubkey: nonced_authority_pubkey,
            authority: 0,
            new_authority_signer: None,
        }],
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Validated(Source::Cluster, blockhash),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        custodian: None,
        no_wait: false,
        compute_unit_price,
    };
    process_command(&config).await.unwrap();
    let stake_account = rpc_client.get_account(&stake_account_pubkey).await.unwrap();
    let stake_state: StakeStateV2 = stake_account.state().unwrap();
    let current_authority = match stake_state {
        StakeStateV2::Initialized(meta) => meta.authorized.staker,
        _ => panic!("Unexpected stake state!"),
    };
    assert_eq!(current_authority, nonced_authority_pubkey);

    // Create nonce account
    let minimum_nonce_balance = rpc_client
        .get_minimum_balance_for_rent_exemption(NonceState::size())
        .await
        .unwrap();
    let nonce_account = Keypair::new();
    config.signers = vec![&default_signer, &nonce_account];
    config.command = CliCommand::CreateNonceAccount {
        nonce_account: 1,
        seed: None,
        nonce_authority: Some(offline_authority_pubkey),
        memo: None,
        amount: SpendAmount::Some(minimum_nonce_balance),
        compute_unit_price,
    };
    process_command(&config).await.unwrap();

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

    // Nonced assignment of new online stake authority
    let online_authority = Keypair::new();
    let online_authority_pubkey = online_authority.pubkey();
    config_offline.signers.push(&nonced_authority);
    config_offline.command = CliCommand::StakeAuthorize {
        stake_account_pubkey,
        new_authorizations: vec![StakeAuthorizationIndexed {
            authorization_type: StakeAuthorize::Staker,
            new_authority_pubkey: online_authority_pubkey,
            authority: 1,
            new_authority_signer: None,
        }],
        sign_only: true,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Static(nonce_hash),
        nonce_account: Some(nonce_account.pubkey()),
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        custodian: None,
        no_wait: false,
        compute_unit_price,
    };
    let sign_reply = process_command(&config_offline).await.unwrap();
    let sign_only = parse_sign_only_reply_string(&sign_reply);
    assert!(sign_only.has_all_signers());
    assert_eq!(sign_only.blockhash, nonce_hash);
    let offline_presigner = sign_only.presigner_of(&offline_authority_pubkey).unwrap();
    let nonced_authority_presigner = sign_only.presigner_of(&nonced_authority_pubkey).unwrap();
    config.signers = vec![&offline_presigner, &nonced_authority_presigner];
    config.command = CliCommand::StakeAuthorize {
        stake_account_pubkey,
        new_authorizations: vec![StakeAuthorizationIndexed {
            authorization_type: StakeAuthorize::Staker,
            new_authority_pubkey: online_authority_pubkey,
            authority: 1,
            new_authority_signer: None,
        }],
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Validated(
            Source::NonceAccount(nonce_account.pubkey()),
            sign_only.blockhash,
        ),
        nonce_account: Some(nonce_account.pubkey()),
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        custodian: None,
        no_wait: false,
        compute_unit_price,
    };
    process_command(&config).await.unwrap();
    let stake_account = rpc_client.get_account(&stake_account_pubkey).await.unwrap();
    let stake_state: StakeStateV2 = stake_account.state().unwrap();
    let current_authority = match stake_state {
        StakeStateV2::Initialized(meta) => meta.authorized.staker,
        _ => panic!("Unexpected stake state!"),
    };
    assert_eq!(current_authority, online_authority_pubkey);

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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_stake_authorize_with_fee_payer() {
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
    let default_pubkey = default_signer.pubkey();

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    config.signers = vec![&default_signer];

    let payer_keypair = keypair_from_seed(&[0u8; 32]).unwrap();
    let mut config_payer = CliConfig::recent_for_tests();
    config_payer.signers = vec![&payer_keypair];
    config_payer.json_rpc_url = test_validator.rpc_url();
    let payer_pubkey = config_payer.signers[0].pubkey();

    let mut config_offline = CliConfig::recent_for_tests();
    let offline_signer = Keypair::new();
    config_offline.signers = vec![&offline_signer];
    config_offline.json_rpc_url = String::new();
    let offline_pubkey = config_offline.signers[0].pubkey();
    // Verify we're offline
    config_offline.command = CliCommand::ClusterVersion;
    process_command(&config_offline).await.unwrap_err();

    request_and_confirm_airdrop(&rpc_client, &config, &default_pubkey, 5_000_000_000_000)
        .await
        .unwrap();
    check_balance!(5_000_000_000_000, &rpc_client, &config.signers[0].pubkey());

    request_and_confirm_airdrop(&rpc_client, &config_payer, &payer_pubkey, 5_000_000_000_000)
        .await
        .unwrap();
    check_balance!(5_000_000_000_000, &rpc_client, &payer_pubkey);

    request_and_confirm_airdrop(
        &rpc_client,
        &config_offline,
        &offline_pubkey,
        5_000_000_000_000,
    )
    .await
    .unwrap();
    check_balance!(5_000_000_000_000, &rpc_client, &offline_pubkey);

    check_ready(&rpc_client).await;

    // Create stake account, identity is authority
    let stake_keypair = Keypair::new();
    let stake_account_pubkey = stake_keypair.pubkey();
    config.signers.push(&stake_keypair);
    config.command = CliCommand::CreateStakeAccount {
        stake_account: 1,
        seed: None,
        staker: None,
        withdrawer: None,
        withdrawer_signer: None,
        lockup: Lockup::default(),
        amount: SpendAmount::Some(1_000_000_000_000),
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Rpc(Source::Cluster),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        from: 0,
        compute_unit_price: None,
    };
    process_command(&config).await.unwrap();
    check_balance!(
        4_000_000_000_000 - fee_two_sig,
        &rpc_client,
        &default_pubkey
    );

    // Assign authority with separate fee payer
    config.signers = vec![&default_signer, &payer_keypair];
    config.command = CliCommand::StakeAuthorize {
        stake_account_pubkey,
        new_authorizations: vec![StakeAuthorizationIndexed {
            authorization_type: StakeAuthorize::Staker,
            new_authority_pubkey: offline_pubkey,
            authority: 0,
            new_authority_signer: None,
        }],
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Rpc(Source::Cluster),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 1,
        custodian: None,
        no_wait: false,
        compute_unit_price: None,
    };
    process_command(&config).await.unwrap();
    // `config` balance has not changed, despite submitting the TX
    check_balance!(
        4_000_000_000_000 - fee_two_sig,
        &rpc_client,
        &default_pubkey
    );
    // `config_payer` however has paid `config`'s authority sig
    // and `config_payer`'s fee sig
    check_balance!(5_000_000_000_000 - fee_two_sig, &rpc_client, &payer_pubkey);

    // Assign authority with offline fee payer
    let blockhash = rpc_client.get_latest_blockhash().await.unwrap();
    config_offline.command = CliCommand::StakeAuthorize {
        stake_account_pubkey,
        new_authorizations: vec![StakeAuthorizationIndexed {
            authorization_type: StakeAuthorize::Staker,
            new_authority_pubkey: payer_pubkey,
            authority: 0,
            new_authority_signer: None,
        }],
        sign_only: true,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Static(blockhash),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        custodian: None,
        no_wait: false,
        compute_unit_price: None,
    };
    config_offline.output_format = OutputFormat::JsonCompact;
    let sign_reply = process_command(&config_offline).await.unwrap();
    let sign_only = parse_sign_only_reply_string(&sign_reply);
    assert!(sign_only.has_all_signers());
    let offline_presigner = sign_only.presigner_of(&offline_pubkey).unwrap();
    config.signers = vec![&offline_presigner];
    config.command = CliCommand::StakeAuthorize {
        stake_account_pubkey,
        new_authorizations: vec![StakeAuthorizationIndexed {
            authorization_type: StakeAuthorize::Staker,
            new_authority_pubkey: payer_pubkey,
            authority: 0,
            new_authority_signer: None,
        }],
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Validated(Source::Cluster, blockhash),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        custodian: None,
        no_wait: false,
        compute_unit_price: None,
    };
    process_command(&config).await.unwrap();
    // `config`'s balance again has not changed
    check_balance!(
        4_000_000_000_000 - fee_two_sig,
        &rpc_client,
        &default_pubkey
    );
    // `config_offline` however has paid 1 sig due to being both authority
    // and fee payer
    check_balance!(
        5_000_000_000_000 - fee_one_sig,
        &rpc_client,
        &offline_pubkey
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[test_case(None; "base")]
#[test_case(Some(1_000_000); "with_compute_unit_price")]
async fn test_stake_split(compute_unit_price: Option<u64>) {
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
    let offline_signer = Keypair::new();

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    config.signers = vec![&default_signer];

    let minimum_balance = rpc_client
        .get_minimum_balance_for_rent_exemption(StakeStateV2::size_of())
        .await
        .unwrap();

    let mut config_offline = CliConfig::recent_for_tests();
    config_offline.json_rpc_url = String::default();
    config_offline.signers = vec![&offline_signer];
    let offline_pubkey = config_offline.signers[0].pubkey();
    // Verify we're offline
    config_offline.command = CliCommand::ClusterVersion;
    process_command(&config_offline).await.unwrap_err();

    request_and_confirm_airdrop(
        &rpc_client,
        &config,
        &config.signers[0].pubkey(),
        50_000_000_000_000,
    )
    .await
    .unwrap();
    check_balance!(50_000_000_000_000, &rpc_client, &config.signers[0].pubkey());

    request_and_confirm_airdrop(
        &rpc_client,
        &config_offline,
        &offline_pubkey,
        1_000_000_000_000,
    )
    .await
    .unwrap();
    check_balance!(1_000_000_000_000, &rpc_client, &offline_pubkey);

    // Create stake account, identity is authority
    let stake_balance = minimum_balance + 10_000_000_000;
    let stake_keypair = keypair_from_seed(&[0u8; 32]).unwrap();
    let stake_account_pubkey = stake_keypair.pubkey();
    config.signers.push(&stake_keypair);
    config.command = CliCommand::CreateStakeAccount {
        stake_account: 1,
        seed: None,
        staker: Some(offline_pubkey),
        withdrawer: Some(offline_pubkey),
        withdrawer_signer: None,
        lockup: Lockup::default(),
        amount: SpendAmount::Some(10 * stake_balance),
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Rpc(Source::Cluster),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        from: 0,
        compute_unit_price,
    };
    process_command(&config).await.unwrap();
    check_balance!(10 * stake_balance, &rpc_client, &stake_account_pubkey,);

    // Create nonce account
    let minimum_nonce_balance = rpc_client
        .get_minimum_balance_for_rent_exemption(NonceState::size())
        .await
        .unwrap();
    let nonce_account = keypair_from_seed(&[1u8; 32]).unwrap();
    config.signers = vec![&default_signer, &nonce_account];
    config.command = CliCommand::CreateNonceAccount {
        nonce_account: 1,
        seed: None,
        nonce_authority: Some(offline_pubkey),
        memo: None,
        amount: SpendAmount::Some(minimum_nonce_balance),
        compute_unit_price,
    };
    process_command(&config).await.unwrap();
    check_balance!(minimum_nonce_balance, &rpc_client, &nonce_account.pubkey());

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

    // Nonced offline split
    let split_account = keypair_from_seed(&[2u8; 32]).unwrap();
    check_balance!(0, &rpc_client, &split_account.pubkey());
    config_offline.signers.push(&split_account);
    config_offline.command = CliCommand::SplitStake {
        stake_account_pubkey,
        stake_authority: 0,
        sign_only: true,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Static(nonce_hash),
        nonce_account: Some(nonce_account.pubkey()),
        nonce_authority: 0,
        memo: None,
        split_stake_account: 1,
        seed: None,
        lamports: 2 * stake_balance,
        fee_payer: 0,
        compute_unit_price,
        rent_exempt_reserve: Some(minimum_balance),
    };
    config_offline.output_format = OutputFormat::JsonCompact;
    let sig_response = process_command(&config_offline).await.unwrap();
    let sign_only = parse_sign_only_reply_string(&sig_response);
    assert!(sign_only.has_all_signers());
    let offline_presigner = sign_only.presigner_of(&offline_pubkey).unwrap();
    config.signers = vec![&offline_presigner, &split_account];
    config.command = CliCommand::SplitStake {
        stake_account_pubkey,
        stake_authority: 0,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Validated(
            Source::NonceAccount(nonce_account.pubkey()),
            sign_only.blockhash,
        ),
        nonce_account: Some(nonce_account.pubkey()),
        nonce_authority: 0,
        memo: None,
        split_stake_account: 1,
        seed: None,
        lamports: 2 * stake_balance,
        fee_payer: 0,
        compute_unit_price,
        rent_exempt_reserve: None,
    };
    process_command(&config).await.unwrap();
    check_balance!(8 * stake_balance, &rpc_client, &stake_account_pubkey);
    check_balance!(
        2 * stake_balance + minimum_balance,
        &rpc_client,
        &split_account.pubkey()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[test_case(None; "base")]
#[test_case(Some(1_000_000); "with_compute_unit_price")]
async fn test_stake_set_lockup(compute_unit_price: Option<u64>) {
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
    let offline_signer = Keypair::new();

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    config.signers = vec![&default_signer];

    let mut config_offline = CliConfig::recent_for_tests();
    config_offline.json_rpc_url = String::default();
    config_offline.signers = vec![&offline_signer];
    let offline_pubkey = config_offline.signers[0].pubkey();
    // Verify we're offline
    config_offline.command = CliCommand::ClusterVersion;
    process_command(&config_offline).await.unwrap_err();

    request_and_confirm_airdrop(
        &rpc_client,
        &config,
        &config.signers[0].pubkey(),
        5_000_000_000_000,
    )
    .await
    .unwrap();
    check_balance!(5_000_000_000_000, &rpc_client, &config.signers[0].pubkey());

    request_and_confirm_airdrop(
        &rpc_client,
        &config_offline,
        &offline_pubkey,
        1_000_000_000_000,
    )
    .await
    .unwrap();
    check_balance!(1_000_000_000_000, &rpc_client, &offline_pubkey);

    // Create stake account, identity is authority
    let stake_balance = rpc_client
        .get_minimum_balance_for_rent_exemption(StakeStateV2::size_of())
        .await
        .unwrap()
        + 10_000_000_000;

    let stake_keypair = keypair_from_seed(&[0u8; 32]).unwrap();
    let stake_account_pubkey = stake_keypair.pubkey();

    let lockup = Lockup {
        custodian: config.signers[0].pubkey(),
        ..Lockup::default()
    };

    config.signers.push(&stake_keypair);
    config.command = CliCommand::CreateStakeAccount {
        stake_account: 1,
        seed: None,
        staker: Some(offline_pubkey),
        withdrawer: Some(config.signers[0].pubkey()),
        withdrawer_signer: None,
        lockup,
        amount: SpendAmount::Some(10 * stake_balance),
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Rpc(Source::Cluster),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        from: 0,
        compute_unit_price,
    };
    process_command(&config).await.unwrap();
    check_balance!(10 * stake_balance, &rpc_client, &stake_account_pubkey,);

    // Online set lockup
    let lockup = LockupArgs {
        unix_timestamp: Some(1_581_534_570),
        epoch: Some(200),
        custodian: None,
    };
    config.signers.pop();
    config.command = CliCommand::StakeSetLockup {
        stake_account_pubkey,
        lockup,
        new_custodian_signer: None,
        custodian: 0,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::default(),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        compute_unit_price,
    };
    process_command(&config).await.unwrap();
    let stake_account = rpc_client.get_account(&stake_account_pubkey).await.unwrap();
    let stake_state: StakeStateV2 = stake_account.state().unwrap();
    let current_lockup = match stake_state {
        StakeStateV2::Initialized(meta) => meta.lockup,
        _ => panic!("Unexpected stake state!"),
    };
    assert_eq!(
        current_lockup.unix_timestamp,
        lockup.unix_timestamp.unwrap()
    );
    assert_eq!(current_lockup.epoch, lockup.epoch.unwrap());
    assert_eq!(current_lockup.custodian, config.signers[0].pubkey());

    // Set custodian to another pubkey
    let online_custodian = Keypair::new();
    let online_custodian_pubkey = online_custodian.pubkey();

    let lockup = LockupArgs {
        unix_timestamp: Some(1_581_534_571),
        epoch: Some(201),
        custodian: Some(online_custodian_pubkey),
    };
    config.command = CliCommand::StakeSetLockup {
        stake_account_pubkey,
        lockup,
        new_custodian_signer: None,
        custodian: 0,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::default(),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        compute_unit_price,
    };
    process_command(&config).await.unwrap();

    let lockup = LockupArgs {
        unix_timestamp: Some(1_581_534_572),
        epoch: Some(202),
        custodian: None,
    };
    config.signers = vec![&default_signer, &online_custodian];
    config.command = CliCommand::StakeSetLockup {
        stake_account_pubkey,
        lockup,
        new_custodian_signer: None,
        custodian: 1,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::default(),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        compute_unit_price,
    };
    process_command(&config).await.unwrap();
    let stake_account = rpc_client.get_account(&stake_account_pubkey).await.unwrap();
    let stake_state: StakeStateV2 = stake_account.state().unwrap();
    let current_lockup = match stake_state {
        StakeStateV2::Initialized(meta) => meta.lockup,
        _ => panic!("Unexpected stake state!"),
    };
    assert_eq!(
        current_lockup.unix_timestamp,
        lockup.unix_timestamp.unwrap()
    );
    assert_eq!(current_lockup.epoch, lockup.epoch.unwrap());
    assert_eq!(current_lockup.custodian, online_custodian_pubkey);

    // Set custodian to offline pubkey
    let lockup = LockupArgs {
        unix_timestamp: Some(1_581_534_573),
        epoch: Some(203),
        custodian: Some(offline_pubkey),
    };
    config.command = CliCommand::StakeSetLockup {
        stake_account_pubkey,
        lockup,
        new_custodian_signer: None,
        custodian: 1,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::default(),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        compute_unit_price,
    };
    process_command(&config).await.unwrap();

    // Create nonce account
    let minimum_nonce_balance = rpc_client
        .get_minimum_balance_for_rent_exemption(NonceState::size())
        .await
        .unwrap();
    let nonce_account = keypair_from_seed(&[1u8; 32]).unwrap();
    let nonce_account_pubkey = nonce_account.pubkey();
    config.signers = vec![&default_signer, &nonce_account];
    config.command = CliCommand::CreateNonceAccount {
        nonce_account: 1,
        seed: None,
        nonce_authority: Some(offline_pubkey),
        memo: None,
        amount: SpendAmount::Some(minimum_nonce_balance),
        compute_unit_price,
    };
    process_command(&config).await.unwrap();
    check_balance!(minimum_nonce_balance, &rpc_client, &nonce_account_pubkey);

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

    // Nonced offline set lockup
    let lockup = LockupArgs {
        unix_timestamp: Some(1_581_534_576),
        epoch: Some(222),
        custodian: None,
    };
    config_offline.command = CliCommand::StakeSetLockup {
        stake_account_pubkey,
        lockup,
        new_custodian_signer: None,
        custodian: 0,
        sign_only: true,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Static(nonce_hash),
        nonce_account: Some(nonce_account_pubkey),
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        compute_unit_price,
    };
    config_offline.output_format = OutputFormat::JsonCompact;
    let sig_response = process_command(&config_offline).await.unwrap();
    let sign_only = parse_sign_only_reply_string(&sig_response);
    assert!(sign_only.has_all_signers());
    let offline_presigner = sign_only.presigner_of(&offline_pubkey).unwrap();
    config.signers = vec![&offline_presigner];
    config.command = CliCommand::StakeSetLockup {
        stake_account_pubkey,
        lockup,
        new_custodian_signer: None,
        custodian: 0,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Validated(
            Source::NonceAccount(nonce_account_pubkey),
            sign_only.blockhash,
        ),
        nonce_account: Some(nonce_account_pubkey),
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        compute_unit_price,
    };
    process_command(&config).await.unwrap();
    let stake_account = rpc_client.get_account(&stake_account_pubkey).await.unwrap();
    let stake_state: StakeStateV2 = stake_account.state().unwrap();
    let current_lockup = match stake_state {
        StakeStateV2::Initialized(meta) => meta.lockup,
        _ => panic!("Unexpected stake state!"),
    };
    assert_eq!(
        current_lockup.unix_timestamp,
        lockup.unix_timestamp.unwrap()
    );
    assert_eq!(current_lockup.epoch, lockup.epoch.unwrap());
    assert_eq!(current_lockup.custodian, offline_pubkey);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[test_case(None; "base")]
#[test_case(Some(1_000_000); "with_compute_unit_price")]
async fn test_offline_nonced_create_stake_account_and_withdraw(compute_unit_price: Option<u64>) {
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
    let mut config = CliConfig::recent_for_tests();
    let default_signer = keypair_from_seed(&[1u8; 32]).unwrap();
    config.signers = vec![&default_signer];
    config.json_rpc_url = test_validator.rpc_url();

    let mut config_offline = CliConfig::recent_for_tests();
    let offline_signer = keypair_from_seed(&[2u8; 32]).unwrap();
    config_offline.signers = vec![&offline_signer];
    let offline_pubkey = config_offline.signers[0].pubkey();
    config_offline.json_rpc_url = String::default();
    config_offline.command = CliCommand::ClusterVersion;
    // Verify that we cannot reach the cluster
    process_command(&config_offline).await.unwrap_err();

    request_and_confirm_airdrop(
        &rpc_client,
        &config,
        &config.signers[0].pubkey(),
        200_000_000_000,
    )
    .await
    .unwrap();
    check_balance!(200_000_000_000, &rpc_client, &config.signers[0].pubkey());

    request_and_confirm_airdrop(
        &rpc_client,
        &config_offline,
        &offline_pubkey,
        100_000_000_000,
    )
    .await
    .unwrap();
    check_balance!(100_000_000_000, &rpc_client, &offline_pubkey);

    // Create nonce account
    let minimum_nonce_balance = rpc_client
        .get_minimum_balance_for_rent_exemption(NonceState::size())
        .await
        .unwrap();
    let nonce_account = keypair_from_seed(&[3u8; 32]).unwrap();
    let nonce_pubkey = nonce_account.pubkey();
    config.signers.push(&nonce_account);
    config.command = CliCommand::CreateNonceAccount {
        nonce_account: 1,
        seed: None,
        nonce_authority: Some(offline_pubkey),
        memo: None,
        amount: SpendAmount::Some(minimum_nonce_balance),
        compute_unit_price,
    };
    process_command(&config).await.unwrap();

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

    // Create stake account offline
    let stake_keypair = keypair_from_seed(&[4u8; 32]).unwrap();
    let stake_pubkey = stake_keypair.pubkey();
    config_offline.signers.push(&stake_keypair);
    config_offline.command = CliCommand::CreateStakeAccount {
        stake_account: 1,
        seed: None,
        staker: None,
        withdrawer: None,
        withdrawer_signer: None,
        lockup: Lockup::default(),
        amount: SpendAmount::Some(50_000_000_000),
        sign_only: true,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Static(nonce_hash),
        nonce_account: Some(nonce_pubkey),
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        from: 0,
        compute_unit_price,
    };
    config_offline.output_format = OutputFormat::JsonCompact;
    let sig_response = process_command(&config_offline).await.unwrap();
    let sign_only = parse_sign_only_reply_string(&sig_response);
    assert!(sign_only.has_all_signers());
    let offline_presigner = sign_only.presigner_of(&offline_pubkey).unwrap();
    let stake_presigner = sign_only.presigner_of(&stake_pubkey).unwrap();
    config.signers = vec![&offline_presigner, &stake_presigner];
    config.command = CliCommand::CreateStakeAccount {
        stake_account: 1,
        seed: None,
        staker: Some(offline_pubkey),
        withdrawer: None,
        withdrawer_signer: None,
        lockup: Lockup::default(),
        amount: SpendAmount::Some(50_000_000_000),
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Validated(
            Source::NonceAccount(nonce_pubkey),
            sign_only.blockhash,
        ),
        nonce_account: Some(nonce_pubkey),
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        from: 0,
        compute_unit_price,
    };
    process_command(&config).await.unwrap();
    check_balance!(50_000_000_000, &rpc_client, &stake_pubkey);

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

    // Offline, nonced stake-withdraw
    let recipient = keypair_from_seed(&[5u8; 32]).unwrap();
    let recipient_pubkey = recipient.pubkey();
    config_offline.signers.pop();
    config_offline.command = CliCommand::WithdrawStake {
        stake_account_pubkey: stake_pubkey,
        destination_account_pubkey: recipient_pubkey,
        amount: SpendAmount::Some(50_000_000_000),
        withdraw_authority: 0,
        custodian: None,
        sign_only: true,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Static(nonce_hash),
        nonce_account: Some(nonce_pubkey),
        nonce_authority: 0,
        memo: None,
        seed: None,
        fee_payer: 0,
        compute_unit_price,
    };
    let sig_response = process_command(&config_offline).await.unwrap();
    let sign_only = parse_sign_only_reply_string(&sig_response);
    let offline_presigner = sign_only.presigner_of(&offline_pubkey).unwrap();
    config.signers = vec![&offline_presigner];
    config.command = CliCommand::WithdrawStake {
        stake_account_pubkey: stake_pubkey,
        destination_account_pubkey: recipient_pubkey,
        amount: SpendAmount::Some(50_000_000_000),
        withdraw_authority: 0,
        custodian: None,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Validated(
            Source::NonceAccount(nonce_pubkey),
            sign_only.blockhash,
        ),
        nonce_account: Some(nonce_pubkey),
        nonce_authority: 0,
        memo: None,
        seed: None,
        fee_payer: 0,
        compute_unit_price,
    };
    process_command(&config).await.unwrap();
    check_balance!(50_000_000_000, &rpc_client, &recipient_pubkey);

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

    // Create another stake account. This time with seed
    let seed = "seedy";
    config_offline.signers = vec![&offline_signer, &stake_keypair];
    config_offline.command = CliCommand::CreateStakeAccount {
        stake_account: 1,
        seed: Some(seed.to_string()),
        staker: None,
        withdrawer: None,
        withdrawer_signer: None,
        lockup: Lockup::default(),
        amount: SpendAmount::Some(50_000_000_000),
        sign_only: true,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Static(nonce_hash),
        nonce_account: Some(nonce_pubkey),
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        from: 0,
        compute_unit_price,
    };
    let sig_response = process_command(&config_offline).await.unwrap();
    let sign_only = parse_sign_only_reply_string(&sig_response);
    let offline_presigner = sign_only.presigner_of(&offline_pubkey).unwrap();
    let stake_presigner = sign_only.presigner_of(&stake_pubkey).unwrap();
    config.signers = vec![&offline_presigner, &stake_presigner];
    config.command = CliCommand::CreateStakeAccount {
        stake_account: 1,
        seed: Some(seed.to_string()),
        staker: Some(offline_pubkey),
        withdrawer: Some(offline_pubkey),
        withdrawer_signer: None,
        lockup: Lockup::default(),
        amount: SpendAmount::Some(50_000_000_000),
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Validated(
            Source::NonceAccount(nonce_pubkey),
            sign_only.blockhash,
        ),
        nonce_account: Some(nonce_pubkey),
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        from: 0,
        compute_unit_price,
    };
    process_command(&config).await.unwrap();
    let seed_address =
        Pubkey::create_with_seed(&stake_pubkey, seed, &stake::program::id()).unwrap();
    check_balance!(50_000_000_000, &rpc_client, &seed_address);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_stake_checked_instructions() {
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
    let default_signer = Keypair::new();

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    config.signers = vec![&default_signer];

    request_and_confirm_airdrop(
        &rpc_client,
        &config,
        &config.signers[0].pubkey(),
        100_000_000_000,
    )
    .await
    .unwrap();

    // Create stake account with withdrawer
    let stake_keypair = Keypair::new();
    let stake_account_pubkey = stake_keypair.pubkey();
    let withdrawer_keypair = Keypair::new();
    let withdrawer_pubkey = withdrawer_keypair.pubkey();
    config.signers.push(&stake_keypair);
    config.command = CliCommand::CreateStakeAccount {
        stake_account: 1,
        seed: None,
        staker: None,
        withdrawer: Some(withdrawer_pubkey),
        withdrawer_signer: Some(1),
        lockup: Lockup::default(),
        amount: SpendAmount::Some(50_000_000_000),
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Rpc(Source::Cluster),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        from: 0,
        compute_unit_price: None,
    };
    process_command(&config).await.unwrap_err(); // unsigned authority should fail

    config.signers = vec![&default_signer, &stake_keypair, &withdrawer_keypair];
    config.command = CliCommand::CreateStakeAccount {
        stake_account: 1,
        seed: None,
        staker: None,
        withdrawer: Some(withdrawer_pubkey),
        withdrawer_signer: Some(1),
        lockup: Lockup::default(),
        amount: SpendAmount::Some(50_000_000_000),
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::Rpc(Source::Cluster),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        from: 0,
        compute_unit_price: None,
    };
    process_command(&config).await.unwrap();

    // Re-authorize account, checking new authority
    let staker_keypair = Keypair::new();
    let staker_pubkey = staker_keypair.pubkey();
    config.signers = vec![&default_signer];
    config.command = CliCommand::StakeAuthorize {
        stake_account_pubkey,
        new_authorizations: vec![StakeAuthorizationIndexed {
            authorization_type: StakeAuthorize::Staker,
            new_authority_pubkey: staker_pubkey,
            authority: 0,
            new_authority_signer: Some(0),
        }],
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::default(),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        custodian: None,
        no_wait: false,
        compute_unit_price: None,
    };
    process_command(&config).await.unwrap_err(); // unsigned authority should fail

    config.signers = vec![&default_signer, &staker_keypair];
    config.command = CliCommand::StakeAuthorize {
        stake_account_pubkey,
        new_authorizations: vec![StakeAuthorizationIndexed {
            authorization_type: StakeAuthorize::Staker,
            new_authority_pubkey: staker_pubkey,
            authority: 0,
            new_authority_signer: Some(1),
        }],
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::default(),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        custodian: None,
        no_wait: false,
        compute_unit_price: None,
    };
    process_command(&config).await.unwrap();
    let stake_account = rpc_client.get_account(&stake_account_pubkey).await.unwrap();
    let stake_state: StakeStateV2 = stake_account.state().unwrap();
    let current_authority = match stake_state {
        StakeStateV2::Initialized(meta) => meta.authorized.staker,
        _ => panic!("Unexpected stake state!"),
    };
    assert_eq!(current_authority, staker_pubkey);

    let new_withdrawer_keypair = Keypair::new();
    let new_withdrawer_pubkey = new_withdrawer_keypair.pubkey();
    config.signers = vec![&default_signer, &withdrawer_keypair];
    config.command = CliCommand::StakeAuthorize {
        stake_account_pubkey,
        new_authorizations: vec![StakeAuthorizationIndexed {
            authorization_type: StakeAuthorize::Withdrawer,
            new_authority_pubkey: new_withdrawer_pubkey,
            authority: 1,
            new_authority_signer: Some(1),
        }],
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::default(),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        custodian: None,
        no_wait: false,
        compute_unit_price: None,
    };
    process_command(&config).await.unwrap_err(); // unsigned authority should fail

    config.signers = vec![
        &default_signer,
        &withdrawer_keypair,
        &new_withdrawer_keypair,
    ];
    config.command = CliCommand::StakeAuthorize {
        stake_account_pubkey,
        new_authorizations: vec![StakeAuthorizationIndexed {
            authorization_type: StakeAuthorize::Withdrawer,
            new_authority_pubkey: new_withdrawer_pubkey,
            authority: 1,
            new_authority_signer: Some(2),
        }],
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::default(),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        custodian: None,
        no_wait: false,
        compute_unit_price: None,
    };
    process_command(&config).await.unwrap();
    let stake_account = rpc_client.get_account(&stake_account_pubkey).await.unwrap();
    let stake_state: StakeStateV2 = stake_account.state().unwrap();
    let current_authority = match stake_state {
        StakeStateV2::Initialized(meta) => meta.authorized.withdrawer,
        _ => panic!("Unexpected stake state!"),
    };
    assert_eq!(current_authority, new_withdrawer_pubkey);

    // Set lockup, checking new custodian
    let custodian = Keypair::new();
    let custodian_pubkey = custodian.pubkey();
    let lockup = LockupArgs {
        unix_timestamp: Some(1_581_534_570),
        epoch: Some(200),
        custodian: Some(custodian_pubkey),
    };
    config.signers = vec![&default_signer, &new_withdrawer_keypair];
    config.command = CliCommand::StakeSetLockup {
        stake_account_pubkey,
        lockup,
        new_custodian_signer: Some(1),
        custodian: 1,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::default(),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        compute_unit_price: None,
    };
    process_command(&config).await.unwrap_err(); // unsigned new custodian should fail

    config.signers = vec![&default_signer, &new_withdrawer_keypair, &custodian];
    config.command = CliCommand::StakeSetLockup {
        stake_account_pubkey,
        lockup,
        new_custodian_signer: Some(2),
        custodian: 1,
        sign_only: false,
        dump_transaction_message: false,
        blockhash_query: BlockhashQuery::default(),
        nonce_account: None,
        nonce_authority: 0,
        memo: None,
        fee_payer: 0,
        compute_unit_price: None,
    };
    process_command(&config).await.unwrap();
    let stake_account = rpc_client.get_account(&stake_account_pubkey).await.unwrap();
    let stake_state: StakeStateV2 = stake_account.state().unwrap();
    let current_lockup = match stake_state {
        StakeStateV2::Initialized(meta) => meta.lockup,
        _ => panic!("Unexpected stake state!"),
    };
    assert_eq!(
        current_lockup.unix_timestamp,
        lockup.unix_timestamp.unwrap()
    );
    assert_eq!(current_lockup.epoch, lockup.epoch.unwrap());
    assert_eq!(current_lockup.custodian, custodian_pubkey);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_stake_minimum_delegation() {
    let mint_keypair = Keypair::new();
    let test_validator =
        TestValidator::async_with_no_fees(&mint_keypair, None, SocketAddrSpace::Unspecified).await;
    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();

    config.command = CliCommand::StakeMinimumDelegation {
        use_lamports_unit: true,
    };

    let result = process_command(&config).await;
    assert_matches!(result, Ok(..));
}
