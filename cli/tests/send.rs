#![allow(clippy::arithmetic_side_effects)]

use {
    serde_json::json,
    solana_cli::cli::{CliCommand, CliConfig, process_command, request_and_confirm_airdrop},
    solana_commitment_config::CommitmentConfig,
    solana_faucet::faucet::run_local_faucet_with_unique_port_for_tests,
    solana_keypair::Keypair,
    solana_native_token::LAMPORTS_PER_SOL,
    solana_net_utils::SocketAddrSpace,
    solana_pubkey::Pubkey,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_signer::Signer,
    solana_system_interface::instruction as system_instruction,
    solana_test_validator::TestValidator,
    std::io::Write,
    tempfile::NamedTempFile,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_send_json_message() {
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
    let signer = Keypair::new();
    let recipient = Pubkey::new_unique();
    request_and_confirm_airdrop(
        &rpc_client,
        &CliConfig::recent_for_tests(),
        &signer.pubkey(),
        LAMPORTS_PER_SOL,
    )
    .await
    .unwrap();

    let transfer_amount = LAMPORTS_PER_SOL / 2;
    let transfer = system_instruction::transfer(&signer.pubkey(), &recipient, transfer_amount);
    let accounts = transfer
        .accounts
        .iter()
        .map(|account| {
            json!({
                "pubkey": account.pubkey.to_string(),
                "isSigner": account.is_signer,
                "isWritable": account.is_writable,
            })
        })
        .collect::<Vec<_>>();
    let mut message_file = NamedTempFile::new().unwrap();
    serde_json::to_writer(
        &mut message_file,
        &json!({
            "instructions": [{
                "programId": transfer.program_id.to_string(),
                "accounts": accounts,
                "data": transfer.data,
            }],
        }),
    )
    .unwrap();
    message_file.flush().unwrap();

    let mut config = CliConfig::recent_for_tests();
    config.json_rpc_url = test_validator.rpc_url();
    config.signers = vec![&signer];
    config.command = CliCommand::Send {
        message_file: message_file.path().to_string_lossy().into_owned(),
    };
    process_command(&config).await.unwrap();

    assert_eq!(
        rpc_client.get_balance(&recipient).await.unwrap(),
        transfer_amount
    );
}
