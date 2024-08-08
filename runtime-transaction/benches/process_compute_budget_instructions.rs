#![feature(test)]
extern crate test;
use {
    solana_runtime_transaction::instructions_processor::process_compute_budget_instructions,
    solana_sdk::{
        compute_budget::ComputeBudgetInstruction,
        instruction::Instruction,
        message::Message,
        pubkey::Pubkey,
        signature::Keypair,
        signer::Signer,
        system_instruction::{self},
        transaction::{SanitizedTransaction, Transaction},
    },
    test::Bencher,
};

const NUM_TRANSACTIONS_PER_ITER: usize = 1024;
const DUMMY_PROGRAM_ID: &str = "dummmy1111111111111111111111111111111111111";

fn build_sanitized_transaction(
    payer_keypair: &Keypair,
    instructions: &[Instruction],
) -> SanitizedTransaction {
    SanitizedTransaction::from_transaction_for_tests(Transaction::new_unsigned(Message::new(
        instructions,
        Some(&payer_keypair.pubkey()),
    )))
}

#[bench]
fn bench_process_compute_budget_instructions_empty(bencher: &mut Bencher) {
    let tx = build_sanitized_transaction(&Keypair::new(), &[]);
    bencher.iter(|| {
        (0..NUM_TRANSACTIONS_PER_ITER).for_each(|_| {
            assert!(
                process_compute_budget_instructions(tx.message().program_instructions_iter())
                    .is_ok()
            )
        })
    });
}

#[bench]
fn bench_process_compute_budget_instructions_non_builtins(bencher: &mut Bencher) {
    let ixs: Vec<_> = (0..4)
        .map(|_| Instruction::new_with_bincode(DUMMY_PROGRAM_ID.parse().unwrap(), &0_u8, vec![]))
        .collect();
    assert_eq!(4, ixs.len());
    let tx = build_sanitized_transaction(&Keypair::new(), &ixs);
    bencher.iter(|| {
        (0..NUM_TRANSACTIONS_PER_ITER).for_each(|_| {
            assert!(
                process_compute_budget_instructions(tx.message().program_instructions_iter())
                    .is_ok()
            )
        })
    });
}

#[bench]
fn bench_process_compute_budget_instructions_compute_budgets(bencher: &mut Bencher) {
    let ixs = vec![
        ComputeBudgetInstruction::request_heap_frame(40 * 1024),
        ComputeBudgetInstruction::set_compute_unit_limit(u32::MAX),
        ComputeBudgetInstruction::set_compute_unit_price(u64::MAX),
        ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(u32::MAX),
    ];
    assert_eq!(4, ixs.len());
    let tx = build_sanitized_transaction(&Keypair::new(), &ixs);
    bencher.iter(|| {
        (0..NUM_TRANSACTIONS_PER_ITER).for_each(|_| {
            assert!(
                process_compute_budget_instructions(tx.message().program_instructions_iter())
                    .is_ok()
            )
        })
    });
}

#[bench]
fn bench_process_compute_budget_instructions_builtins(bencher: &mut Bencher) {
    let ixs = vec![
        Instruction::new_with_bincode(solana_sdk::bpf_loader::id(), &0_u8, vec![]),
        Instruction::new_with_bincode(solana_sdk::secp256k1_program::id(), &0_u8, vec![]),
        Instruction::new_with_bincode(
            solana_sdk::address_lookup_table::program::id(),
            &0_u8,
            vec![],
        ),
        Instruction::new_with_bincode(solana_sdk::loader_v4::id(), &0_u8, vec![]),
    ];
    assert_eq!(4, ixs.len());
    let tx = build_sanitized_transaction(&Keypair::new(), &ixs);
    bencher.iter(|| {
        (0..NUM_TRANSACTIONS_PER_ITER).for_each(|_| {
            assert!(
                process_compute_budget_instructions(tx.message().program_instructions_iter())
                    .is_ok()
            )
        })
    });
}
#[bench]
fn bench_process_compute_budget_instructions_mixed(bencher: &mut Bencher) {
    let payer_keypair = Keypair::new();
    let mut ixs: Vec<_> = (0..128)
        .map(|_| Instruction::new_with_bincode(DUMMY_PROGRAM_ID.parse().unwrap(), &0_u8, vec![]))
        .collect();
    ixs.extend(vec![
        ComputeBudgetInstruction::request_heap_frame(40 * 1024),
        ComputeBudgetInstruction::set_compute_unit_limit(u32::MAX),
        ComputeBudgetInstruction::set_compute_unit_price(u64::MAX),
        ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(u32::MAX),
        system_instruction::transfer(&payer_keypair.pubkey(), &Pubkey::new_unique(), 1),
    ]);
    assert_eq!(133, ixs.len());
    let tx = build_sanitized_transaction(&payer_keypair, &ixs);

    bencher.iter(|| {
        (0..NUM_TRANSACTIONS_PER_ITER).for_each(|_| {
            assert!(
                process_compute_budget_instructions(tx.message().program_instructions_iter())
                    .is_ok()
            )
        })
    });
}
