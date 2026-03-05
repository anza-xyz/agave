use {
    solana_cli::cli::{CliCommand, CliConfig, process_command},
    solana_cli_output::{CliValidators, CliValidatorsSortOrder, OutputFormat},
    solana_keypair::Keypair,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    std::sync::Arc,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_show_validators() {
    let keypair = Keypair::new();
    let mut config = CliConfig {
        rpc_client: Some(Arc::new(RpcClient::new_mock("succeeds".to_string()))),
        json_rpc_url: "http://127.0.0.1:8899".to_string(),
        output_format: OutputFormat::JsonCompact,
        ..CliConfig::default()
    };
    config.signers = vec![&keypair];

    // Run show-validators with default options
    config.command = CliCommand::ShowValidators {
        use_lamports_unit: false,
        sort_order: CliValidatorsSortOrder::default(),
        reverse_sort: false,
        number_validators: false,
        keep_unstaked_delinquents: true,
        delinquent_slot_distance: None,
    };
    let response = process_command(&config).await.unwrap();
    let result: CliValidators = serde_json::from_str(&response).unwrap();

    // The mock RPC returns one delinquent validator
    assert_eq!(result.validators.len(), 1);

    let validator = &result.validators[0];

    // The mock vote account has inflation_rewards_commission_bps=Some(10000) which
    // maps to 10000 bps (100.00%)
    assert_eq!(validator.commission_bps, 10000);

    // Mock returns the validator as delinquent with last_vote=0
    assert!(validator.delinquent);
    assert_eq!(validator.last_vote, 0);

    // Verify pubkeys are populated
    assert!(!validator.identity_pubkey.is_empty());
    assert!(!validator.vote_account_pubkey.is_empty());

    // No current validators, so all stake is delinquent
    assert_eq!(result.total_current_stake, 0);
    assert_eq!(result.total_active_stake, result.total_delinquent_stake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_show_validators_sort_by_commission() {
    let keypair = Keypair::new();
    let mut config = CliConfig {
        rpc_client: Some(Arc::new(RpcClient::new_mock("succeeds".to_string()))),
        json_rpc_url: "http://127.0.0.1:8899".to_string(),
        output_format: OutputFormat::JsonCompact,
        ..CliConfig::default()
    };
    config.signers = vec![&keypair];

    // Run show-validators sorted by commission with lamports display
    config.command = CliCommand::ShowValidators {
        use_lamports_unit: true,
        sort_order: CliValidatorsSortOrder::Commission,
        reverse_sort: false,
        number_validators: true,
        keep_unstaked_delinquents: true,
        delinquent_slot_distance: None,
    };
    let response = process_command(&config).await.unwrap();
    let result: CliValidators = serde_json::from_str(&response).unwrap();

    assert_eq!(result.validators.len(), 1);

    let validator = &result.validators[0];
    assert_eq!(validator.commission_bps, 10000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_show_validators_reverse_sort() {
    let keypair = Keypair::new();
    let mut config = CliConfig {
        rpc_client: Some(Arc::new(RpcClient::new_mock("succeeds".to_string()))),
        json_rpc_url: "http://127.0.0.1:8899".to_string(),
        output_format: OutputFormat::JsonCompact,
        ..CliConfig::default()
    };
    config.signers = vec![&keypair];

    // Run show-validators with reverse sort
    config.command = CliCommand::ShowValidators {
        use_lamports_unit: false,
        sort_order: CliValidatorsSortOrder::default(),
        reverse_sort: true,
        number_validators: false,
        keep_unstaked_delinquents: true,
        delinquent_slot_distance: None,
    };
    let response = process_command(&config).await.unwrap();
    let result: CliValidators = serde_json::from_str(&response).unwrap();

    // Should still return the same single validator
    assert_eq!(result.validators.len(), 1);
    assert_eq!(result.validators[0].commission_bps, 10000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_show_validators_display_output() {
    let keypair = Keypair::new();
    let mut config = CliConfig {
        rpc_client: Some(Arc::new(RpcClient::new_mock("succeeds".to_string()))),
        json_rpc_url: "http://127.0.0.1:8899".to_string(),
        output_format: OutputFormat::Display,
        ..CliConfig::default()
    };
    config.signers = vec![&keypair];

    config.command = CliCommand::ShowValidators {
        use_lamports_unit: false,
        sort_order: CliValidatorsSortOrder::default(),
        reverse_sort: false,
        number_validators: false,
        keep_unstaked_delinquents: true,
        delinquent_slot_distance: None,
    };
    let output = process_command(&config).await.unwrap();
    // Strip ANSI escape codes (bold on/off) and warning emoji for readable comparison
    let output = output
        .replace("\u{26a0}\u{fe0f}", "!")
        .replace("\x1b[1m", "")
        .replace("\x1b[0m", "");
    let expected = concat!(
        "   Identity                                      Vote Account                              Commission  Last Vote        Root Slot     Skip Rate  Credits  Version Client Id                Active Stake\n",
        "! 7RoSF9fUmdphVCpabEoefH81WwrW7orsWonXWqTXkKV8  7RoSF9fUmdphVCpabEoefH81WwrW7orsWonXWqTXkKV8  100.00%          -                -          -           0  unknown Agave                 0.000000000 SOL (NaN%)\n",
        "\n",
        "Average Stake-Weighted Skip Rate: 100.00%\n",
        "Average Unweighted Skip Rate:     100.00%\n",
        "\n",
        "Active Stake: 0 SOL\n",
        "\n",
        "Stake By Version:\n",
        "unknown -    0 current validators (  NaN%)   1 delinquent validators (  NaN%)\n",
        "\n",
        "Stake By Client ID:\n",
        "Agave          -    0 current validators (  NaN%)   1 delinquent validators (  NaN%)\n",
    );
    assert_eq!(output, expected);
}
