#![cfg(feature = "sbf_rust")]

use {
    agave_feature_set::disable_fees_sysvar,
    solana_instruction::{AccountMeta, Instruction},
    solana_message::Message,
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::{Bank, SlotLeader},
        bank_client::BankClient,
        genesis_utils::{GenesisConfigInfo, create_genesis_config},
        loader_utils::create_program,
    },
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_sdk_ids::{
        bpf_loader_upgradeable,
        sysvar::{
            clock, epoch_schedule, instructions, recent_blockhashes, rent, slot_hashes,
            slot_history, stake_history,
        },
    },
    solana_signer::Signer,
    solana_stake_interface::stake_history::{StakeHistory, StakeHistoryEntry},
    solana_sysvar::epoch_rewards,
    solana_transaction::Transaction,
};

#[test]
fn test_sysvar_syscalls() {
    agave_logger::setup();

    let GenesisConfigInfo {
        mut genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);
    genesis_config.accounts.remove(&disable_fees_sysvar::id());

    let bank = Bank::new_for_tests(&genesis_config);

    let epoch_rewards = epoch_rewards::EpochRewards {
        distribution_starting_block_height: 42,
        total_rewards: 100,
        distributed_rewards: 50,
        active: true,
        ..epoch_rewards::EpochRewards::default()
    };
    bank.set_sysvar_for_tests(&epoch_rewards);

    let stake_history = {
        let mut stake_history = StakeHistory::default();
        stake_history.add(
            0,
            StakeHistoryEntry {
                effective: 200,
                activating: 300,
                deactivating: 400,
            },
        );
        stake_history
    };
    bank.set_sysvar_for_tests(&stake_history);

    let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
    let program_id = create_program(
        &bank,
        &bpf_loader_upgradeable::id(),
        "solana_sbf_rust_sysvar",
    );
    let mut bank_client = BankClient::new_shared(bank.clone());
    let bank = bank_client
        .advance_slot(1, &bank_forks, SlotLeader::default())
        .unwrap();
    let dummy_account_key = Pubkey::new_unique();
    bank.store_account(
        &dummy_account_key,
        &solana_account::AccountSharedData::new(1, 32, &program_id),
    );
    bank.freeze();
    let blockhash = bank.last_blockhash();

    for ix_discriminator in 0..4 {
        let instruction = Instruction::new_with_bincode(
            program_id,
            &[ix_discriminator],
            vec![
                AccountMeta::new(mint_keypair.pubkey(), true),
                AccountMeta::new(dummy_account_key, false),
                AccountMeta::new_readonly(clock::id(), false),
                AccountMeta::new_readonly(epoch_schedule::id(), false),
                AccountMeta::new_readonly(instructions::id(), false),
                #[allow(deprecated)]
                AccountMeta::new_readonly(recent_blockhashes::id(), false),
                AccountMeta::new_readonly(rent::id(), false),
                AccountMeta::new_readonly(slot_hashes::id(), false),
                AccountMeta::new_readonly(slot_history::id(), false),
                AccountMeta::new_readonly(stake_history::id(), false),
                AccountMeta::new_readonly(epoch_rewards::id(), false),
            ],
        );
        let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
        let transaction = Transaction::new(&[&mint_keypair], message, blockhash);
        let sanitized_tx = RuntimeTransaction::from_transaction_for_tests(transaction);
        let result = bank.simulate_transaction(&sanitized_tx, false);
        assert!(result.result.is_ok());
    }

    // Storing the result of get_sysvar() in the input region is not allowed
    // because of the 16 byte alignment requirement of the EpochRewards sysvar.
    let instruction = Instruction::new_with_bincode(
        program_id,
        &[4],
        vec![
            AccountMeta::new(mint_keypair.pubkey(), true),
            AccountMeta::new_readonly(epoch_rewards::id(), false),
            AccountMeta::new(dummy_account_key, false),
        ],
    );
    let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
    let transaction = Transaction::new(&[&mint_keypair], message, blockhash);
    let sanitized_tx = RuntimeTransaction::from_transaction_for_tests(transaction);
    let result = bank.simulate_transaction(&sanitized_tx, false);
    assert!(result.result.is_err());
}
