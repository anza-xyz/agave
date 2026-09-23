#![cfg(feature = "sbf_rust")]

use {
    solana_instruction::Instruction,
    solana_leader_schedule::LeaderSchedule,
    solana_message::Message,
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::{Bank, SlotLeader},
        bank_client::BankClient,
        epoch_stakes::VersionedEpochStakes,
        genesis_utils::{
            GenesisConfigInfo, ValidatorVoteKeypairs, create_genesis_config_with_vote_accounts,
        },
        loader_utils::create_program,
        stakes::SerdeStakesToStakeFormat,
    },
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_sdk_ids::bpf_loader_upgradeable,
    solana_signer::Signer,
    solana_transaction::Transaction,
    std::num::NonZeroUsize,
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

    let mut bank = Bank::new_for_tests(&genesis_config);
    assert_eq!(bank.slot(), 0);

    let current = SlotLeader {
        id: Pubkey::new_unique(),
        vote_address: Pubkey::new_unique(),
    };
    let next = SlotLeader {
        id: Pubkey::new_unique(),
        vote_address: Pubkey::new_unique(),
    };

    // The child bank is slot 1, so `next_leader` reads the schedule at slot 2.
    let (epoch, slot_index) = bank.epoch_schedule().get_epoch_and_slot_index(2);
    let slot_index = usize::try_from(slot_index).unwrap();
    let mut slot_leaders = vec![SlotLeader::default(); slot_index];
    slot_leaders.push(next);
    let leader_schedule =
        LeaderSchedule::new_from_schedule(slot_leaders, NonZeroUsize::new(1).unwrap());
    let stakes = SerdeStakesToStakeFormat::from(bank.get_top_epoch_stakes());
    bank.set_epoch_stakes_for_test(
        epoch,
        VersionedEpochStakes::new(stakes, epoch, Some(leader_schedule)),
    );

    let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
    let program_id = create_program(
        &bank,
        &bpf_loader_upgradeable::id(),
        "solana_sbf_syscall_get_leader",
    );
    let mut bank_client = BankClient::new_shared(bank);
    let bank = bank_client.advance_slot(1, &bank_forks, current).unwrap();
    bank.freeze();
    assert_eq!(bank.leader(), &current);

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

    let data = &result.return_data.unwrap().data;
    assert_eq!(data.len(), 128);
    assert_eq!(read_pubkey(data, 0), current.id);
    assert_eq!(read_pubkey(data, 32), next.id);
    assert_eq!(read_pubkey(data, 64), current.vote_address);
    assert_eq!(read_pubkey(data, 96), next.vote_address);
}

fn read_pubkey(data: &[u8], offset: usize) -> Pubkey {
    let end = offset.saturating_add(32);
    Pubkey::new_from_array(data[offset..end].try_into().unwrap())
}
