#![feature(test)]
#![allow(clippy::arithmetic_side_effects)]

extern crate test;

use {
    criterion::{criterion_group, criterion_main, Criterion},
    rand::{seq::SliceRandom, thread_rng},
    solana_accounts_db::{accounts::Accounts, accounts_db::AccountsDb},
    solana_sdk::{
        account::AccountSharedData,
        instruction::{AccountMeta, Instruction},
        pubkey::Pubkey,
        system_program,
        transaction::{SanitizedTransaction, Transaction, MAX_TX_ACCOUNT_LOCKS},
    },
    std::sync::Arc,
};

// simultaneous transactions locked
const BATCH_SIZES: [usize; 3] = [1, 32, 64];

// locks acquired per transaction
const LOCK_COUNTS: [usize; 2] = [2, 64];

// largest batch size * largest lock count * 2
const ACCOUNTS_DB_SIZE: usize = 8192;

fn create_test_data(batch_size: usize, lock_count: usize) -> (Accounts, Vec<SanitizedTransaction>) {
    let accounts_db = AccountsDb::new_single_for_tests();
    let accounts = Accounts::new(Arc::new(accounts_db));

    let mut all_account_pubkeys = vec![];
    let mut transactions = vec![];

    for _ in 0..batch_size {
        let mut account_metas = vec![];

        // `lock_accounts()` distinguishes writable from readonly, so give transactions an even split
        // signer doesnt matter for locking but `sanitize()` expects to see at least one
        for i in 0..lock_count {
            let pubkey = Pubkey::new_unique();

            let account_meta = if i % 2 == 0 {
                AccountMeta::new(pubkey, true)
            } else {
                AccountMeta::new_readonly(pubkey, false)
            };

            account_metas.push(account_meta);
            all_account_pubkeys.push(pubkey);
        }

        let instruction = Instruction::new_with_bincode(system_program::id(), &(), account_metas);
        let transaction = Transaction::new_with_payer(&[instruction], None);

        transactions.push(SanitizedTransaction::from_transaction_for_tests(
            transaction,
        ));
    }

    // fill out the rest so all batches have the same size accounts-db
    for _ in all_account_pubkeys.len()..ACCOUNTS_DB_SIZE {
        all_account_pubkeys.push(Pubkey::new_unique());
    }
    assert_eq!(all_account_pubkeys.len(), ACCOUNTS_DB_SIZE);

    // and shuffle before constructing accounts-db so lookup is random
    all_account_pubkeys.shuffle(&mut thread_rng());

    let account = AccountSharedData::new(1, 0, &Pubkey::default());
    for pubkey in all_account_pubkeys {
        accounts.store_slow_uncached(0, &pubkey, &account);
    }

    (accounts, transactions)
}

fn bench_entry_lock_accounts(c: &mut Criterion) {
    let mut group = c.benchmark_group("bench_lock_accounts");

    for batch_size in BATCH_SIZES {
        for lock_count in LOCK_COUNTS {
            let name = format!("batch_size_{batch_size}_locks_count_{lock_count}");
            let (accounts, transactions) = create_test_data(batch_size, lock_count);

            group.bench_function(name.as_str(), move |b| {
                b.iter(|| {
                    let results = accounts.lock_accounts(transactions.iter(), MAX_TX_ACCOUNT_LOCKS);
                    accounts.unlock_accounts(transactions.iter().zip(&results));
                })
            });
        }
    }
}

criterion_group!(benches, bench_entry_lock_accounts);
criterion_main!(benches);
