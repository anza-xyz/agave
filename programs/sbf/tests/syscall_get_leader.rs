#![cfg(feature = "sbf_rust")]

use {
    solana_instruction::Instruction,
    solana_message::Message,
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::{Bank, SlotLeader, SlotLeaders},
        bank_client::BankClient,
        genesis_utils::{
            GenesisConfigInfo, ValidatorVoteKeypairs, create_genesis_config_with_vote_accounts,
        },
        loader_utils::create_program,
    },
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_sdk_ids::bpf_loader_upgradeable,
    solana_signer::Signer,
    solana_transaction::Transaction,
};

#[test]
fn test_syscall_get_leader() {
    agave_logger::setup();

    let voting_keypairs = vec![ValidatorVoteKeypairs::new_rand()];
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_vote_accounts(
        1_000_000_000,
        &voting_keypairs,
        vec![100_000_000],
    );

    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let program_id = create_program(
        &bank,
        &bpf_loader_upgradeable::id(),
        "solana_sbf_syscall_get_leader",
    );

    // The leaders arrive with the bank, the way the block creation loop and
    // replay stage supply them from the leader schedule cache.
    let leaders = SlotLeaders::new_unchecked(SlotLeader::new_unique(), SlotLeader::new_unique());
    let mut bank_client = BankClient::new_shared(bank);
    let bank = bank_client.advance_slot(1, &bank_forks, leaders).unwrap();
    bank.freeze();
    assert_eq!(bank.leader(), leaders.cur_leader());

    let mint_pubkey = mint_keypair.pubkey();
    let message = Message::new(
        &[Instruction::new_with_bytes(program_id, &[], vec![])],
        Some(&mint_pubkey),
    );
    let transaction = Transaction::new(&[&mint_keypair], message, bank.last_blockhash());
    let sanitized_tx = RuntimeTransaction::from_transaction_for_tests(transaction);
    let result = bank.simulate_transaction(&sanitized_tx, false);
    assert!(
        result.result.is_ok(),
        "sol_get_leader failed: {:?}",
        result.result
    );

    // `LeaderInfo` layout (SIMD-0558): leader identity, leader vote, next
    // leader identity, next leader vote.
    let cur_leader = leaders.cur_leader();
    let next_leader = leaders.next_leader();
    let data = &result.return_data.unwrap().data;
    assert_eq!(data.len(), 128);
    assert_eq!(read_pubkey(data, 0), cur_leader.id);
    assert_eq!(read_pubkey(data, 32), cur_leader.vote_address);
    assert_eq!(read_pubkey(data, 64), next_leader.id);
    assert_eq!(read_pubkey(data, 96), next_leader.vote_address);
}

fn read_pubkey(data: &[u8], offset: usize) -> Pubkey {
    let end = offset.saturating_add(32);
    Pubkey::new_from_array(data[offset..end].try_into().unwrap())
}
