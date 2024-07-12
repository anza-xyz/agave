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
        system_program, system_instruction,
        transaction::{Transaction, SanitizedTransaction, MAX_TX_ACCOUNT_LOCKS},
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

fn create_transactions(bank: &Bank, num: usize, num_transfers: usize) -> Vec<SanitizedTransaction> {
    let funded_accounts = create_funded_accounts(bank, num * (num_transfers * 2));
    funded_accounts
        .into_par_iter()
        .chunks(num_transfers + 1)
        .map(|chunk| {
            let from = &chunk[0];
            let transfer_instructions = chunk[1..]
                .iter()
                .map(|to| system_instruction::transfer(&from.pubkey(), &to.pubkey(), 1))
                .collect::<Vec<_>>();

            let mut transaction = Transaction::new_with_payer(
                &transfer_instructions,
                Some(&from.pubkey()),
            );

            transaction.sign(&[&from], bank.last_blockhash());
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

fn bench_lock_accounts(
    bencher: &mut Bencher,
    batch_size: usize,
    num_accounts: usize,
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
    let transactions = create_transactions(&bank, 2_usize.pow(16),num_accounts/2);
    let mut batches = transactions.chunks(batch_size).cycle();
    bencher.iter(|| {
        for batch in (0..batches_per_iteration).filter_map(|_| batches.next()) {
            let (results,_) = bank
                .rc
                .accounts
                .lock_accounts(test::black_box(batch.iter()), MAX_TX_ACCOUNT_LOCKS,allow_self_conflicting_entries,);
            bank.rc.accounts.unlock_accounts(batch.iter().zip(&results));
        }
    });
}

#[bench]
fn bench_lock_accounts_unbatched_2_accounts_self_conflicting_entries_allowed(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 1, 2, true);
}

#[bench]
fn bench_lock_accounts_half_batch_2_accounts_self_conflicting_entries_allowed(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 32, 2, true);
}

#[bench]
fn bench_lock_accounts_full_batch_2_accounts_self_conflicting_entries_allowed(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 64, 2, true);
}

#[bench]
fn bench_lock_accounts_unbatched_2_accounts(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 1, 2, false);
}

#[bench]
fn bench_lock_accounts_half_batch_2_accounts(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 32, 2, false);
}

#[bench]
fn bench_lock_accounts_full_batch_2_accounts(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 64, 2, false);
}

#[bench]
fn bench_lock_accounts_unbatched_4_accounts_self_conflicting_entries_allowed(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 1, 4, true);
}

#[bench]
fn bench_lock_accounts_half_batch_4_accounts_self_conflicting_entries_allowed(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 32, 4, true);
}

#[bench]
fn bench_lock_accounts_full_batch_4_accounts_self_conflicting_entries_allowed(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 64, 4, true);
}

#[bench]
fn bench_lock_accounts_unbatched_4_accounts(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 1, 4, false);
}

#[bench]
fn bench_lock_accounts_half_batch_4_accounts(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 32, 4, false);
}

#[bench]
fn bench_lock_accounts_full_batch_4_accounts(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 64, 4, false);
}

#[bench]
fn bench_lock_accounts_unbatched_8_accounts_self_conflicting_entries_allowed(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 1, 8, true);
}

#[bench]
fn bench_lock_accounts_half_batch_8_accounts_self_conflicting_entries_allowed(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 32, 8, true);
}

#[bench]
fn bench_lock_accounts_full_batch_8_accounts_self_conflicting_entries_allowed(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 64, 8, true);
}

#[bench]
fn bench_lock_accounts_unbatched_8_accounts(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 1, 8, false);
}

#[bench]
fn bench_lock_accounts_half_batch_8_accounts(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 32, 8, false);
}

#[bench]
fn bench_lock_accounts_full_batch_8_accounts(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 64, 8, false);
}

#[bench]
fn bench_lock_accounts_unbatched_16_accounts_self_conflicting_entries_allowed(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 1, 16, true);
}

#[bench]
fn bench_lock_accounts_half_batch_16_accounts_self_conflicting_entries_allowed(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 32, 16, true);
}

#[bench]
fn bench_lock_accounts_full_batch_16_accounts_self_conflicting_entries_allowed(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 64, 16, true);
}

#[bench]
fn bench_lock_accounts_unbatched_16_accounts(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 1, 16, false);
}

#[bench]
fn bench_lock_accounts_half_batch_16_accounts(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 32, 16, false);
}

#[bench]
fn bench_lock_accounts_full_batch_16_accounts(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 64, 16, false);
}

#[bench]
fn bench_lock_accounts_unbatched_32_accounts_self_conflicting_entries_allowed(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 1, 32, true);
}

#[bench]
fn bench_lock_accounts_half_batch_32_accounts_self_conflicting_entries_allowed(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 32, 32, true);
}

#[bench]
fn bench_lock_accounts_full_batch_32_accounts_self_conflicting_entries_allowed(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 64, 32, true);
}

#[bench]
fn bench_lock_accounts_unbatched_32_accounts(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 1, 32, false);
}

#[bench]
fn bench_lock_accounts_half_batch_32_accounts(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 32, 32, false);
}

#[bench]
fn bench_lock_accounts_full_batch_32_accounts(bencher: &mut Bencher) {
    bench_lock_accounts(bencher, 64, 32, false);
}