#![allow(clippy::arithmetic_side_effects)]
#![feature(test)]

use {
    criterion::{criterion_group, criterion_main, Criterion},
    rayon::{
        iter::IndexedParallelIterator,
        prelude::{IntoParallelIterator, IntoParallelRefIterator, ParallelIterator},
    },
    solana_ledger::genesis_utils::{create_genesis_config, GenesisConfigInfo},
    solana_runtime::bank::Bank,
    solana_sdk::{
        account::{Account, ReadableAccount},
        instruction::{AccountMeta, Instruction},
        signature::Keypair,
        signer::Signer,
        stake_history::Epoch,
        system_program,
        transaction::{SanitizedTransaction, Transaction, MAX_TX_ACCOUNT_LOCKS},
    },
    std::sync::Arc,
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

fn create_transactions(bank: &Bank, num: usize, lock_count: usize) -> Vec<SanitizedTransaction> {
    let total_account_locks = if lock_count == 1 {
        // special case where writable_accounts=2 and readable_accounts=0
        2
    } else {
        2 * lock_count
    };
    let funded_accounts = create_funded_accounts(bank, num * (total_account_locks));
    funded_accounts
        .into_par_iter()
        .chunks(total_account_locks)
        .map(|chunk| {
            let (writable_accounts, readable_accounts);
            let accounts = if lock_count == 1 {
                // special case where writable_accounts=2 and readable_accounts=0
                let lock_count = lock_count * 2;
                writable_accounts = &chunk[..lock_count];
                writable_accounts
                    .iter()
                    .map(|account| AccountMeta::new(account.pubkey(), true))
                    .collect()
            } else {
                writable_accounts = &chunk[..lock_count];
                readable_accounts = &chunk[lock_count..];
                writable_accounts
                    .iter()
                    .map(|account| AccountMeta::new(account.pubkey(), true))
                    .chain(
                        readable_accounts
                            .iter()
                            .map(|account| AccountMeta::new_readonly(account.pubkey(), false)),
                    )
                    .collect::<Vec<_>>()
            };

            let instruction = Instruction::new_with_bincode(
                system_program::id(),
                &(), // instruction data, empty in this case
                accounts,
            );

            let mut transaction = Transaction::new_with_payer(
                &[instruction],
                Some(&writable_accounts[0].pubkey()), // The first writable account is the payer
            );

            transaction.sign(
                &writable_accounts.iter().collect::<Vec<_>>(),
                bank.last_blockhash(),
            );
            transaction
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

// represents the no of txs in the entry
const BATCH_SIZES: [usize; 3] = [1, 32, 64];
// no of readable and writable accounts
// 1 is a special case where writable_accounts=2 and readable_accounts=0
// const LOCK_COUNTS: [usize; 5] = [1, 2, 4, 8, 16];
const LOCK_COUNTS: [usize; 1] = [1];

const TRANSACTIONS_PER_ITERATION: usize = 64;

fn bench_entry_lock_accounts(c: &mut Criterion) {
    let allow_self_conflicting_entries = false;

    let mut group = c.benchmark_group("bench_lock_accounts");
    for batch_size in BATCH_SIZES {
        for lock_count in LOCK_COUNTS {
            assert_eq!(
                TRANSACTIONS_PER_ITERATION % batch_size,
                0,
                "batch_size must be a factor of `TRANSACTIONS_PER_ITERATION` \
                 ({TRANSACTIONS_PER_ITERATION}) so that bench results are easily comparable"
            );
            let batches_per_iteration = TRANSACTIONS_PER_ITERATION / batch_size;

            let bank = bank_setup();
            let transactions = create_transactions(&bank, 2_usize.pow(16), lock_count);
            let mut batches = transactions.chunks(batch_size).cycle();

            let name = if lock_count==1 {
                format!("batch_size_{batch_size}_locks_count_{}_write_only", lock_count*2)
            } else {
                format!("batch_size_{batch_size}_locks_count_{lock_count}")
            };
            
            group.bench_function(name.as_str(), move |b| {
                b.iter(|| {
                    for batch in (0..batches_per_iteration).filter_map(|_| batches.next()) {
                        let (results, _) = bank.rc.accounts.lock_accounts(
                            test::black_box(batch.iter()),
                            MAX_TX_ACCOUNT_LOCKS,
                            allow_self_conflicting_entries,
                        );
                        bank.rc.accounts.unlock_accounts(batch.iter().zip(&results));
                    }
                })
            });
        }
    }
}

fn bench_entry_lock_accounts_with_self_conflicting_txs(c: &mut Criterion) {
    let allow_self_conflicting_entries = true;

    let mut group = c.benchmark_group("bench_lock_accounts");
    for batch_size in BATCH_SIZES {
        for lock_count in LOCK_COUNTS {
            assert_eq!(
                TRANSACTIONS_PER_ITERATION % batch_size,
                0,
                "batch_size must be a factor of `TRANSACTIONS_PER_ITERATION` \
                 ({TRANSACTIONS_PER_ITERATION}) so that bench results are easily comparable"
            );
            let batches_per_iteration = TRANSACTIONS_PER_ITERATION / batch_size;

            let bank = bank_setup();
            let transactions = create_transactions(&bank, 2_usize.pow(16), lock_count);
            let mut batches = transactions.chunks(batch_size).cycle();

            let name = if lock_count==1 {
                format!("batch_size_{batch_size}_locks_count_{}_write_only_self_conflicting_entries_allowed", lock_count*2)
            } else {
                format!("batch_size_{batch_size}_locks_count_{lock_count}_self_conflicting_entries_allowed")
            };
            group.bench_function(name.as_str(), move |b| {
                b.iter(|| {
                    for batch in (0..batches_per_iteration).filter_map(|_| batches.next()) {
                        let (results, _) = bank.rc.accounts.lock_accounts(
                            test::black_box(batch.iter()),
                            MAX_TX_ACCOUNT_LOCKS,
                            allow_self_conflicting_entries,
                        );
                        bank.rc.accounts.unlock_accounts(batch.iter().zip(&results));
                    }
                })
            });
        }
    }
}

criterion_group!(
    benches,
    bench_entry_lock_accounts,
    bench_entry_lock_accounts_with_self_conflicting_txs,
);
criterion_main!(benches);
