use {
    serial_test::serial,
    solana_clock::DEFAULT_MS_PER_SLOT,
    solana_commitment_config::CommitmentConfig,
    solana_keypair::Keypair,
    solana_native_token::sol_str_to_lamports,
    solana_net_utils::SocketAddrSpace,
    solana_pubkey::Pubkey,
    solana_rpc_client::rpc_client::RpcClient,
    solana_signer::Signer,
    solana_test_validator::TestValidator,
    solana_tokens::commands::{
        test_process_create_stake_with_client, test_process_distribute_stake_with_client,
        test_process_distribute_tokens_with_client,
    },
    std::{thread::sleep, time::Duration},
};

fn simple_test_validator(alice: Pubkey) -> TestValidator {
    let test_validator =
        TestValidator::start_with_config(alice, None, SocketAddrSpace::Unspecified);
    // Programs deployed at genesis are not immediately available
    let rpc_client =
        RpcClient::new_with_commitment(test_validator.rpc_url(), CommitmentConfig::processed());
    while rpc_client
        .get_slot_with_commitment(CommitmentConfig::processed())
        .unwrap()
        < 5
    {
        sleep(Duration::from_millis(DEFAULT_MS_PER_SLOT));
    }
    test_validator
}

#[test]
#[serial]
fn test_process_token_allocations() {
    agave_logger::setup();

    let alice = Keypair::new();
    let test_validator = simple_test_validator(alice.pubkey());
    let url = test_validator.rpc_url();

    let client = RpcClient::new_with_commitment(url, CommitmentConfig::processed());
    test_process_distribute_tokens_with_client(&client, alice, None);
}

#[test]
#[serial]
fn test_process_transfer_amount_allocations() {
    let alice = Keypair::new();
    let test_validator = simple_test_validator(alice.pubkey());
    let url = test_validator.rpc_url();

    let client = RpcClient::new_with_commitment(url, CommitmentConfig::processed());
    test_process_distribute_tokens_with_client(&client, alice, sol_str_to_lamports("1.5"));
}

#[test]
#[serial]
fn test_create_stake_allocations() {
    let alice = Keypair::new();
    let test_validator = simple_test_validator(alice.pubkey());
    let url = test_validator.rpc_url();

    let client = RpcClient::new_with_commitment(url, CommitmentConfig::processed());
    test_process_create_stake_with_client(&client, alice);
}

#[test]
#[serial]
fn test_process_stake_allocations() {
    let alice = Keypair::new();
    let test_validator = simple_test_validator(alice.pubkey());
    let url = test_validator.rpc_url();

    let client = RpcClient::new_with_commitment(url, CommitmentConfig::processed());
    test_process_distribute_stake_with_client(&client, alice);
}
