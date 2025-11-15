use {
    agave_feature_set::FeatureSet,
    criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput},
    solana_compute_budget_instruction::instructions_processor::process_compute_budget_instructions,
    solana_compute_budget_interface::ComputeBudgetInstruction,
    solana_instruction::Instruction,
    solana_keypair::Keypair,
    solana_message::Message,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    solana_svm_transaction::svm_message::SVMStaticMessage,
    solana_system_interface::instruction::transfer,
    solana_transaction::{sanitized::SanitizedTransaction, Transaction},
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

fn bench_process_compute_budget_instructions_empty(c: &mut Criterion) {
    for feature_set in [FeatureSet::default(), FeatureSet::all_enabled()] {
        c.benchmark_group("bench_process_compute_budget_instructions_empty")
            .throughput(Throughput::Elements(NUM_TRANSACTIONS_PER_ITER as u64))
            .bench_function("0 instructions", |bencher| {
                let tx = build_sanitized_transaction(&Keypair::new(), &[]);
                bencher.iter(|| {
                    (0..NUM_TRANSACTIONS_PER_ITER).for_each(|_| {
                        assert!(process_compute_budget_instructions(
                            black_box(SVMStaticMessage::program_instructions_iter(&tx)),
                            black_box(&feature_set),
                        )
                        .is_ok())
                    })
                });
            });
    }
}

fn bench_process_compute_budget_instructions_no_builtins(c: &mut Criterion) {
    let num_instructions = 4;
    for feature_set in [FeatureSet::default(), FeatureSet::all_enabled()] {
        c.benchmark_group("bench_process_compute_budget_instructions_no_builtins")
            .throughput(Throughput::Elements(NUM_TRANSACTIONS_PER_ITER as u64))
            .bench_function(
                format!("{num_instructions} dummy Instructions"),
                |bencher| {
                    let ixs: Vec<_> = (0..num_instructions)
                        .map(|_| {
                            Instruction::new_with_bincode(
                                DUMMY_PROGRAM_ID.parse().unwrap(),
                                &(),
                                vec![],
                            )
                        })
                        .collect();
                    let tx = build_sanitized_transaction(&Keypair::new(), &ixs);
                    bencher.iter(|| {
                        (0..NUM_TRANSACTIONS_PER_ITER).for_each(|_| {
                            assert!(process_compute_budget_instructions(
                                black_box(SVMStaticMessage::program_instructions_iter(&tx)),
                                black_box(&feature_set),
                            )
                            .is_ok())
                        })
                    });
                },
            );
    }
}

fn bench_process_compute_budget_instructions_compute_budgets(c: &mut Criterion) {
    for feature_set in [FeatureSet::default(), FeatureSet::all_enabled()] {
        c.benchmark_group("bench_process_compute_budget_instructions_compute_budgets")
            .throughput(Throughput::Elements(NUM_TRANSACTIONS_PER_ITER as u64))
            .bench_function("4 compute-budget instructions", |bencher| {
                let ixs = vec![
                    ComputeBudgetInstruction::request_heap_frame(40 * 1024),
                    ComputeBudgetInstruction::set_compute_unit_limit(u32::MAX),
                    ComputeBudgetInstruction::set_compute_unit_price(u64::MAX),
                    ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(u32::MAX),
                ];
                let tx = build_sanitized_transaction(&Keypair::new(), &ixs);
                bencher.iter(|| {
                    (0..NUM_TRANSACTIONS_PER_ITER).for_each(|_| {
                        assert!(process_compute_budget_instructions(
                            black_box(SVMStaticMessage::program_instructions_iter(&tx)),
                            black_box(&feature_set),
                        )
                        .is_ok())
                    })
                });
            });
    }
}

fn bench_process_compute_budget_instructions_builtins(c: &mut Criterion) {
    for feature_set in [FeatureSet::default(), FeatureSet::all_enabled()] {
        c.benchmark_group("bench_process_compute_budget_instructions_builtins")
            .throughput(Throughput::Elements(NUM_TRANSACTIONS_PER_ITER as u64))
            .bench_function("4 dummy builtins", |bencher| {
                let ixs = vec![
                    Instruction::new_with_bincode(solana_sdk_ids::bpf_loader::id(), &(), vec![]),
                    Instruction::new_with_bincode(
                        solana_sdk_ids::secp256k1_program::id(),
                        &(),
                        vec![],
                    ),
                    Instruction::new_with_bincode(
                        solana_sdk_ids::address_lookup_table::id(),
                        &(),
                        vec![],
                    ),
                    Instruction::new_with_bincode(solana_sdk_ids::loader_v4::id(), &(), vec![]),
                ];
                let tx = build_sanitized_transaction(&Keypair::new(), &ixs);
                bencher.iter(|| {
                    (0..NUM_TRANSACTIONS_PER_ITER).for_each(|_| {
                        assert!(process_compute_budget_instructions(
                            black_box(SVMStaticMessage::program_instructions_iter(&tx)),
                            black_box(&feature_set),
                        )
                        .is_ok())
                    })
                });
            });
    }
}

fn bench_process_compute_budget_instructions_mixed(c: &mut Criterion) {
    let num_instructions = 355;
    for feature_set in [FeatureSet::default(), FeatureSet::all_enabled()] {
        c.benchmark_group("bench_process_compute_budget_instructions_mixed")
            .throughput(Throughput::Elements(NUM_TRANSACTIONS_PER_ITER as u64))
            .bench_function(
                format!("{num_instructions} mixed instructions"),
                |bencher| {
                    let payer_keypair = Keypair::new();
                    let mut ixs: Vec<_> = (0..num_instructions)
                        .map(|_| {
                            Instruction::new_with_bincode(
                                DUMMY_PROGRAM_ID.parse().unwrap(),
                                &(),
                                vec![],
                            )
                        })
                        .collect();
                    ixs.extend(vec![
                        ComputeBudgetInstruction::request_heap_frame(40 * 1024),
                        ComputeBudgetInstruction::set_compute_unit_limit(u32::MAX),
                        ComputeBudgetInstruction::set_compute_unit_price(u64::MAX),
                        ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(u32::MAX),
                        transfer(&payer_keypair.pubkey(), &Pubkey::new_unique(), 1),
                    ]);
                    let tx = build_sanitized_transaction(&payer_keypair, &ixs);

                    bencher.iter(|| {
                        (0..NUM_TRANSACTIONS_PER_ITER).for_each(|_| {
                            assert!(process_compute_budget_instructions(
                                black_box(SVMStaticMessage::program_instructions_iter(&tx)),
                                black_box(&feature_set),
                            )
                            .is_ok())
                        })
                    });
                },
            );
    }
}

criterion_group!(
    benches,
    bench_process_compute_budget_instructions_empty,
    bench_process_compute_budget_instructions_no_builtins,
    bench_process_compute_budget_instructions_compute_budgets,
    bench_process_compute_budget_instructions_builtins,
    bench_process_compute_budget_instructions_mixed,
);
criterion_main!(benches);
