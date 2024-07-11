#![allow(clippy::arithmetic_side_effects)]
#![feature(test)]

use {
    rayon::{
        iter::IndexedParallelIterator,
        prelude::{IntoParallelIterator, IntoParallelRefIterator, ParallelIterator},
    },
    solana_ledger::genesis_utils::{create_genesis_config, GenesisConfigInfo},
    solana_runtime::bank::Bank,
    solana_sdk::{
        account::{Account, ReadableAccount},
        signature::Keypair,
        signer::Signer,
        stake_history::Epoch,
        system_program, system_transaction,
        transaction::{SanitizedTransaction, MAX_TX_ACCOUNT_LOCKS},
    },
    std::sync::Arc,
    test::Bencher,
};

extern crate test;

fn create_accounts(num: usize) -> Vec<Keypair> {
    (0..num).into_par_iter().map(|_| Keypair::new()).collect()
}

fn create_funded_accounts(bank: &Bank, num: usize) -> Vec<Keypair> {
    assert!(
        num.is_power_of_two(),
        "must be power of 2 for parallel funding tree"
    );
    let accounts = create_accounts(num);

    accounts.par_iter().for_each(|account| {
        bank.store_account(
            &account.pubkey(),
            &Account {
                lamports: 5100,
                data: vec![],
                owner: system_program::id(),
                executable: false,
                rent_epoch: Epoch::MAX,
            }
            .to_account_shared_data(),
        );
    });

    accounts
}

fn create_transactions(bank: &Bank, num: usize) -> Vec<SanitizedTransaction> {
    let funded_accounts = create_funded_accounts(bank, 2 * num);
    funded_accounts
        .into_par_iter()
        .chunks(2)
        .map(|chunk| {
            let from = &chunk[0];
            let to = &chunk[1];
            system_transaction::transfer(from, &to.pubkey(), 1, bank.last_blockhash())
        })
        .map(SanitizedTransaction::from_transaction_for_tests)
        .collect()
}

fn bank_setup() -> Arc<Bank> {
    let mint_total = u64::MAX;
    let GenesisConfigInfo {
        mut genesis_config, ..
    } = create_genesis_config(mint_total);

    // Set a high ticks_per_slot so we don't run out of ticks
    // during the benchmark
    genesis_config.ticks_per_slot = 10_000;

    let mut bank = Bank::new_for_benches(&genesis_config);

    // Allow arbitrary transaction processing time for the purposes of this bench
    bank.ns_per_slot = u128::MAX;

    // set cost tracker limits to MAX so it will not filter out TXs
    bank.write_cost_tracker()
        .unwrap()
        .set_limits(u64::MAX, u64::MAX, u64::MAX);
    bank.wrap_with_bank_forks_for_tests().0
}

fn bench_lock_accounts(
    bencher: &mut Bencher,
    batch_size: usize,
    allow_self_conflicting_entries: bool,
) {
    const TRANSACTIONS_PER_ITERATION: usize = 64;
    assert_eq!(
        TRANSACTIONS_PER_ITERATION % batch_size,
        0,
        "batch_size must be a factor of `TRANSACTIONS_PER_ITERATION` \
         ({TRANSACTIONS_PER_ITERATION}) so that bench results are easily comparable"
    );
    let batches_per_iteration = TRANSACTIONS_PER_ITERATION / batch_size;

    let bank = bank_setup();
    let transactions = create_transactions(&bank, 2_usize.pow(20));
    let batches: Vec<_> = transactions.chunks(batch_size).collect();

    bencher.iter(|| {
        for i in 0..batches_per_iteration {
            if let Some(batch) = batches.get(i) {
                let _ = bank.rc.accounts.lock_accounts(
                    (*batch).iter(),
                    MAX_TX_ACCOUNT_LOCKS,
                    allow_self_conflicting_entries,
                );
            }
        }
    });

    // drop batches here so dropping is not included in the benchmark
    drop(batches);
}

#[bench]
fn bench_lock_accounts_unbatched_self_conflicting_entries_allowed(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 1, true);
}

#[bench]
fn bench_lock_accounts_half_batch_self_conflicting_entries_allowed(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 32, true);
}

#[bench]
fn bench_lock_accounts_full_batch_self_conflicting_entries_allowed(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 64, true);
}

#[bench]
fn bench_lock_accounts_unbatched(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 1, false);
}

#[bench]
fn bench_lock_accounts_half_batch(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 32, false);
}

#[bench]
fn bench_lock_accounts_full_batch(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 64, false);
}
