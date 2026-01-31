#![feature(test)]
#![allow(clippy::arithmetic_side_effects)]

extern crate test;

use {
    solana_account::{AccountSharedData, ReadableAccount},
    solana_genesis_config::create_genesis_config,
    solana_instruction::error::LamportsError,
    solana_leader_schedule::SlotLeader,
    solana_pubkey::Pubkey,
    solana_runtime::bank::*,
    std::{path::PathBuf, sync::Arc},
    test::Bencher,
};

fn deposit_many(bank: &Bank, pubkeys: &mut Vec<Pubkey>, num: usize) -> Result<(), LamportsError> {
    for t in 0..num {
        let pubkey = solana_pubkey::new_rand();
        let account =
            AccountSharedData::new((t + 1) as u64, 0, AccountSharedData::default().owner());
        pubkeys.push(pubkey);
        assert!(bank.get_account(&pubkey).is_none());
        test_utils::deposit(bank, &pubkey, (t + 1) as u64)?;
        assert_eq!(bank.get_account(&pubkey).unwrap(), account);
    }
    Ok(())
}

#[bench]
fn bench_accounts_create(bencher: &mut Bencher) {
    let (genesis_config, _) = create_genesis_config(10_000);
    let bank0 = Bank::new_with_paths_for_benches(&genesis_config, vec![PathBuf::from("bench_a0")]);
    bencher.iter(|| {
        let mut pubkeys: Vec<Pubkey> = vec![];
        deposit_many(&bank0, &mut pubkeys, 1000).unwrap();
    });
}

#[bench]
fn bench_accounts_squash(bencher: &mut Bencher) {
    let (genesis_config, _) = create_genesis_config(100_000);
    let mut prev_bank = Arc::new(Bank::new_with_paths_for_benches(
        &genesis_config,
        vec![PathBuf::from("bench_a1")],
    ));
    let mut pubkeys: Vec<Pubkey> = vec![];
    deposit_many(&prev_bank, &mut pubkeys, 250_000).unwrap();
    prev_bank.freeze();

    // Measures the performance of the squash operation.
    // This mainly consists of the freeze operation which calculates the
    // merkle hash of the account state and distribution of fees and rent
    let mut slot = 1u64;
    bencher.iter(|| {
        let next_bank = Arc::new(Bank::new_from_parent(
            prev_bank.clone(),
            SlotLeader::default(),
            slot,
        ));
        test_utils::deposit(&next_bank, &pubkeys[0], 1).unwrap();
        next_bank.squash();
        slot += 1;
        prev_bank = next_bank;
    });
}
