#![allow(clippy::arithmetic_side_effects)]

use {
    criterion::{Criterion, criterion_group, criterion_main},
    solana_account::{AccountSharedData, ReadableAccount},
    solana_genesis_config::create_genesis_config,
    solana_instruction::error::LamportsError,
    solana_leader_schedule::SlotLeader,
    solana_pubkey::Pubkey,
    solana_runtime::bank::*,
    std::{path::PathBuf, sync::Arc, time::Duration},
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

fn bench_accounts_create(c: &mut Criterion) {
    let (genesis_config, _) = create_genesis_config(10_000);
    let bank0 = Bank::new_with_paths_for_benches(&genesis_config, vec![PathBuf::from("bench_a0")]);
    c.bench_function("bench_accounts_create", |b| {
        b.iter(|| {
            let mut pubkeys: Vec<Pubkey> = vec![];
            deposit_many(&bank0, &mut pubkeys, 1000).unwrap();
        })
    });
}

fn bench_accounts_squash(c: &mut Criterion) {
    let (genesis_config, _) = create_genesis_config(100_000);
    let prev_bank =
        Bank::new_with_paths_for_benches(&genesis_config, vec![PathBuf::from("bench_a1")]);
    let (mut prev_bank, _bank_forks) = prev_bank.wrap_with_bank_forks_for_tests();
    let mut pubkeys: Vec<Pubkey> = vec![];
    deposit_many(&prev_bank, &mut pubkeys, 250_000).unwrap();
    prev_bank.freeze();

    // Measures the performance of the squash operation.
    // This mainly consists of the freeze operation which calculates the
    // merkle hash of the account state and distribution of fees and rent
    let mut slot = 1u64;
    c.bench_function("bench_accounts_squash", |b| {
        b.iter(|| {
            let next_bank = Arc::new(Bank::new_from_parent(
                prev_bank.clone(),
                SlotLeader::default(),
                slot,
            ));
            test_utils::deposit(&next_bank, &pubkeys[0], 1).unwrap();
            next_bank.squash();
            slot += 1;
            prev_bank = next_bank;
        })
    });
}

criterion_group! {
    name = benches;
    // Both benches mutate the state they measure - `create` keeps depositing
    // into one bank, `squash` chains a deeper bank per iteration - so cost and
    // memory grow with the iteration count. Short windows bound that drift.
    config = Criterion::default()
        .warm_up_time(Duration::from_millis(250))
        .measurement_time(Duration::from_millis(750))
        .sample_size(10)
        .without_plots();
    targets = bench_accounts_create, bench_accounts_squash
}
criterion_main!(benches);
