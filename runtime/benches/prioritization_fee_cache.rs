#![feature(test)]
extern crate test;

use {
    rand::{rng, Rng},
    solana_compute_budget_interface::ComputeBudgetInstruction,
    solana_message::Message,
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::Bank,
        bank_forks::BankForks,
        genesis_utils::{create_genesis_config, GenesisConfigInfo},
        prioritization_fee_cache::*,
    },
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_system_interface::instruction as system_instruction,
    solana_transaction::{sanitized::SanitizedTransaction, Transaction},
    std::sync::Arc,
    test::Bencher,
};
const TRANSFER_TRANSACTION_COMPUTE_UNIT: u32 = 200;

fn build_sanitized_transaction(
    compute_unit_price: u64,
    signer_account: &Pubkey,
    write_account: &Pubkey,
) -> RuntimeTransaction<SanitizedTransaction> {
    let transfer_lamports = 1;
    let transaction = Transaction::new_unsigned(Message::new(
        &[
            system_instruction::transfer(signer_account, write_account, transfer_lamports),
            ComputeBudgetInstruction::set_compute_unit_limit(TRANSFER_TRANSACTION_COMPUTE_UNIT),
            ComputeBudgetInstruction::set_compute_unit_price(compute_unit_price),
        ],
        Some(signer_account),
    ));

    RuntimeTransaction::from_transaction_for_tests(transaction)
}

#[bench]
#[ignore]
fn bench_process_transactions_single_slot(bencher: &mut Bencher) {
    let prioritization_fee_cache = PrioritizationFeeCache::default();

    let bank = Arc::new(Bank::default_for_tests());

    // build test transactions
    let transactions: Vec<_> = (0..5000)
        .map(|n| {
            let compute_unit_price = n % 7;
            build_sanitized_transaction(
                compute_unit_price,
                &Pubkey::new_unique(),
                &Pubkey::new_unique(),
            )
        })
        .collect();

    bencher.iter(|| {
        prioritization_fee_cache.update(&bank, transactions.iter());
    });
}

fn process_transactions_multiple_slots(banks: &[Arc<Bank>], num_slots: usize, num_threads: usize) {
    let prioritization_fee_cache = Arc::new(PrioritizationFeeCache::default());

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(num_threads)
        .build()
        .unwrap();

    // each threads updates a slot a batch of 50 transactions, for 100 times
    for _ in 0..100 {
        pool.install(|| {
            let transactions: Vec<_> = (0..50)
                .map(|n| {
                    let compute_unit_price = n % 7;
                    build_sanitized_transaction(
                        compute_unit_price,
                        &Pubkey::new_unique(),
                        &Pubkey::new_unique(),
                    )
                })
                .collect();

            let index = rng().random_range(0..num_slots);

            prioritization_fee_cache.update(&banks[index], transactions.iter());
        })
    }
}

#[bench]
#[ignore]
fn bench_process_transactions_multiple_slots(bencher: &mut Bencher) {
    const NUM_SLOTS: usize = 5;
    const NUM_THREADS: usize = 3;

    let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
    let bank0 = Bank::new_for_benches(&genesis_config);
    let bank_forks = BankForks::new_rw_arc(bank0);
    let bank = bank_forks.read().unwrap().working_bank();
    let collector = solana_pubkey::new_rand();
    let banks = (1..=NUM_SLOTS)
        .map(|n| Arc::new(Bank::new_from_parent(bank.clone(), &collector, n as u64)))
        .collect::<Vec<_>>();

    bencher.iter(|| {
        process_transactions_multiple_slots(&banks, NUM_SLOTS, NUM_THREADS);
    });
}
