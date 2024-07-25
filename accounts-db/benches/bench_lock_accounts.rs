use {
    criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput},
    itertools::iproduct,
    solana_accounts_db::{accounts::Accounts, accounts_db::AccountsDb},
    solana_sdk::{
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

// total transactions per run
const TOTAL_TRANSACTIONS: usize = 1024;

fn create_test_transactions(
    lock_count: usize,
    self_conflicting_batch: bool,
) -> Vec<SanitizedTransaction> {
    let shared_pubkeys: Vec<_> = (0..lock_count).map(|_| Pubkey::new_unique()).collect();
    let mut transactions = vec![];

    for _ in 0..TOTAL_TRANSACTIONS {
        let mut account_metas = vec![];

        #[allow(clippy::needless_range_loop)]
        for i in 0..lock_count {
            // `lock_accounts()` distinguishes writable from readonly, so give transactions an even split
            // signer doesnt matter for locking but `sanitize()` expects to see at least one
            let account_meta = if i == 0 {
                AccountMeta::new(Pubkey::new_unique(), true)
            } else if i % 2 == 0 {
                if !self_conflicting_batch {
                    AccountMeta::new(Pubkey::new_unique(), false)
                } else {
                    AccountMeta::new_readonly(shared_pubkeys[i], false)
                }
            } else if self_conflicting_batch {
                AccountMeta::new(shared_pubkeys[i], false)
            } else {
                AccountMeta::new_readonly(shared_pubkeys[i], false)
            };

            account_metas.push(account_meta);
        }

        let instruction = Instruction::new_with_bincode(system_program::id(), &(), account_metas);
        let transaction = Transaction::new_with_payer(&[instruction], None);

        transactions.push(SanitizedTransaction::from_transaction_for_tests(
            transaction,
        ));
    }

    transactions
}

fn bench_entry_lock_accounts(c: &mut Criterion) {
    for (batch_size, lock_count, self_conflicting_batch) in
        iproduct!(BATCH_SIZES, LOCK_COUNTS, [false, true])
    {
        let mut group = if self_conflicting_batch {
            c.benchmark_group("bench_lock_accounts_with_conflicting_batches")
        } else {
            c.benchmark_group("bench_lock_accounts")
        };

        let name = format!("batch_size_{batch_size}_locks_count_{lock_count}");

        let accounts_db = AccountsDb::new_single_for_tests();
        let accounts = Accounts::new(Arc::new(accounts_db));

        let transactions = create_test_transactions(lock_count, self_conflicting_batch);
        group.throughput(Throughput::Elements(transactions.len() as u64));
        let transaction_batches: Vec<_> = transactions.chunks(batch_size).collect();

        group.bench_function(name.as_str(), move |b| {
            b.iter(|| {
                for batch in &transaction_batches {
                    let (results, _) = accounts.lock_accounts(
                        test::black_box(batch.iter()),
                        MAX_TX_ACCOUNT_LOCKS,
                        self_conflicting_batch,
                    );
                    accounts.unlock_accounts(batch.iter().zip(&results));
                }
            })
        });
    }
}

criterion_group!(benches, bench_entry_lock_accounts);
criterion_main!(benches);
