#![cfg(feature = "abi_v2")]

use {
    solana_account::{AccountSharedData, WritableAccount},
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_message::Message,
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::Bank,
        bank_client::BankClient,
        genesis_utils::{create_genesis_config, GenesisConfigInfo},
        loader_utils::load_program_of_loader_v4,
    },
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_signer::Signer,
    solana_transaction::Transaction,
};

#[test]
fn test_abiv2() {
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);

    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mut bank_client = BankClient::new_shared(bank.clone());
    let authority_keypair = Keypair::new();

    let (bank, program_id) = load_program_of_loader_v4(
        &mut bank_client,
        bank_forks.as_ref(),
        &mint_keypair,
        &authority_keypair,
        "solana_sbf_rust_abiv2",
    );

    let mut key_arr = [0u8; 32];
    for (i, item) in key_arr.iter_mut().enumerate() {
        *item = i as u8;
    }
    let account_key = Pubkey::from(key_arr);
    for (i, item) in key_arr.iter_mut().enumerate() {
        *item = 2usize.saturating_mul(i) as u8;
    }

    let owner_key = Pubkey::from(key_arr);
    let other_account_data =
        AccountSharedData::create(789, vec![1, 2, 3, 4, 5], owner_key, false, u64::MAX);
    bank.store_account(&account_key, &other_account_data);

    bank.freeze();

    let account_metas = vec![
        AccountMeta::new(mint_keypair.pubkey(), true),
        AccountMeta::new_readonly(account_key, false),
    ];
    let instruction = Instruction::new_with_bytes(program_id, &[0, 2], account_metas.clone());

    let blockhash = bank.last_blockhash();
    let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
    let transaction = Transaction::new(&[&mint_keypair], message, blockhash);
    let sanitized_tx = RuntimeTransaction::from_transaction_for_tests(transaction);

    let result = bank.simulate_transaction(&sanitized_tx, false);
    assert!(result.result.is_ok());
}
