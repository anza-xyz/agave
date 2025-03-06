use {
    agave_banking_stage_ingress_types::BankingPacketBatch,
    criterion::{black_box, criterion_group, criterion_main, Criterion},
    crossbeam_channel::unbounded,
    solana_core::banking_stage::{
        consumer::TARGET_NUM_TRANSACTIONS_PER_BATCH,
        decision_maker::BufferedPacketsDecision,
        packet_deserializer::PacketDeserializer,
        scheduler_messages::{ConsumeWork, FinishedConsumeWork, TransactionBatchId},
        transaction_scheduler::{
            receive_and_buffer::{
                ReceiveAndBuffer, SanitizedTransactionReceiveAndBuffer,
                TransactionViewReceiveAndBuffer,
            },
            scheduler_metrics::{SchedulerCountMetrics, SchedulerTimingMetrics},
            scheduler_test_utils::{
                create_accounts, generate_transactions, TransactionConfig, UniformDist,
            },
            transaction_state_container::StateContainer,
        },
    },
    solana_keypair::Keypair,
    solana_ledger::genesis_utils::{create_genesis_config, GenesisConfigInfo},
    solana_perf::packet::{to_packet_batches, NUM_PACKETS},
    solana_poh::poh_recorder::BankStart,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_sdk::{
        hash::Hash, message::Message, signer::Signer, system_instruction, transaction::Transaction,
    },
    std::{
        sync::Arc,
        thread,
        time::{Duration, Instant},
    },
};

// How long does it take to create 8192 txs? ~95ms
fn bench_transaction_creation(c: &mut Criterion) {
    let num_txs = NUM_PACKETS;
    let from_keypairs: Vec<Keypair> = (0..num_txs).map(|_| Keypair::new()).collect();
    let to_pubkeys: Vec<Pubkey> = (0..num_txs).map(|_| Pubkey::new_unique()).collect();
    let recent_blockhash = Hash::new_unique();

    c.bench_function("transaction_creation", |bencher| {
        bencher.iter(|| {
            for i in 0..num_txs {
                let transfer =
                    system_instruction::transfer(&from_keypairs[i].pubkey(), &to_pubkeys[i], 1);
                let message = Message::new(&[transfer], Some(&from_keypairs[i].pubkey()));
                black_box(Transaction::new(
                    &vec![&from_keypairs[i]],
                    message,
                    recent_blockhash,
                ));
            }
        })
    });
}
// How long does it take to serialize 8192 txs? ~3ms
fn bench_transaction_serialization(c: &mut Criterion) {
    let num_txs = NUM_PACKETS;
    let from_keypairs: Vec<Keypair> = (0..num_txs).map(|_| Keypair::new()).collect();
    let to_pubkeys: Vec<Pubkey> = (0..num_txs).map(|_| Pubkey::new_unique()).collect();
    let recent_blockhash = Hash::new_unique();

    let txs: Vec<Transaction> = (0..num_txs)
        .map(|i| {
            let transfer =
                system_instruction::transfer(&from_keypairs[i].pubkey(), &to_pubkeys[i], 1);
            let message = Message::new(&[transfer], Some(&from_keypairs[i].pubkey()));
            Transaction::new(&vec![&from_keypairs[i]], message, recent_blockhash)
        })
        .collect();
    c.bench_function("transaction_serialization", |bencher| {
        bencher.iter(|| {
            black_box(BankingPacketBatch::new(to_packet_batches(
                &txs,
                NUM_PACKETS,
            )));
        })
    });
}

fn bench_sanitized_transaction_receive_and_buffer(c: &mut Criterion) {
    let GenesisConfigInfo {
        mut genesis_config, ..
    } = create_genesis_config(100_000);
    let num_txs = 1024 * 64;
    let num_account_conflicts = 1;
    let num_accounts = ((num_account_conflicts + 1) * num_txs) / num_account_conflicts;
    let account_keypairs = create_accounts(num_accounts, &mut genesis_config);

    let (bank, bank_forks) =
        Bank::new_for_benches(&genesis_config).wrap_with_bank_forks_for_tests();
    let bank_start = BankStart {
        working_bank: bank.clone(),
        bank_creation_time: Arc::new(Instant::now()),
    };

    let (sender, receiver) = unbounded();

    let mut rb = SanitizedTransactionReceiveAndBuffer::new(
        PacketDeserializer::new(receiver),
        bank_forks,
        false,
    );

    const TOTAL_BUFFERED_PACKETS: usize = 100_000;
    let mut count_metrics = SchedulerCountMetrics::default();
    let mut timing_metrics = SchedulerTimingMetrics::default();
    let decision = BufferedPacketsDecision::Consume(bank_start);

    c.bench_function("sanitized_transaction_receive_and_buffer", |bencher| {
        bencher.iter_with_setup(
            || {
                generate_transactions(
                    num_txs,
                    &account_keypairs,
                    bank.clone(),
                    sender.clone(),
                    TransactionConfig {
                        compute_unit_price: Box::new(UniformDist::new(1, 100)),
                        transaction_cu_budget: 1, // No effect
                        num_account_conflicts,    // No effect for this benchmark
                        probability_invalid_blockhash: 0.0,
                        probability_invalid_account: 0.0, // No effect
                        data_size_limit: 1,               // No effect
                    },
                );
                let container =
        <SanitizedTransactionReceiveAndBuffer as ReceiveAndBuffer>::Container::with_capacity(
            TOTAL_BUFFERED_PACKETS,
        );
                container
            },
            |mut container| {
                let res = rb.receive_and_buffer_packets(
                    &mut container,
                    &mut timing_metrics,
                    &mut count_metrics,
                    &decision,
                );
                assert!(res.unwrap() == 2 * num_txs && !container.is_empty());
                black_box(container);
            },
        )
    });
}

fn bench_sanitized_transaction_receive_and_buffer2(c: &mut Criterion) {
    let GenesisConfigInfo {
        mut genesis_config, ..
    } = create_genesis_config(100_000);
    let num_txs = 1024 * 64;
    let num_account_conflicts = 1;
    let num_accounts = ((num_account_conflicts + 1) * num_txs) / num_account_conflicts;
    let account_keypairs = create_accounts(num_accounts, &mut genesis_config);

    let (bank, bank_forks) =
        Bank::new_for_benches(&genesis_config).wrap_with_bank_forks_for_tests();
    let bank_start = BankStart {
        working_bank: bank.clone(),
        bank_creation_time: Arc::new(Instant::now()),
    };

    let (sender, receiver) = unbounded();

    let mut rb = SanitizedTransactionReceiveAndBuffer::new(
        PacketDeserializer::new(receiver),
        bank_forks,
        false,
    );

    const TOTAL_BUFFERED_PACKETS: usize = 100_000;
    let mut count_metrics = SchedulerCountMetrics::default();
    let mut timing_metrics = SchedulerTimingMetrics::default();
    let decision = BufferedPacketsDecision::Consume(bank_start);

    c.bench_function("sanitized_transaction_receive_and_buffer", |bencher| {
        bencher.iter_with_setup(
            || {
                generate_transactions(
                    num_txs,
                    &account_keypairs,
                    bank.clone(),
                    sender.clone(),
                    TransactionConfig {
                        compute_unit_price: Box::new(UniformDist::new(1, 100)),
                        transaction_cu_budget: 1, // No effect
                        num_account_conflicts,    // No effect for this benchmark
                        probability_invalid_blockhash: 0.0,
                        probability_invalid_account: 0.0, // No effect
                        data_size_limit: 1,               // No effect
                    },
                );
                let container =
        <SanitizedTransactionReceiveAndBuffer as ReceiveAndBuffer>::Container::with_capacity(
            TOTAL_BUFFERED_PACKETS,
        );
                container
            },
            |mut container| {
                let res = rb.receive_and_buffer_packets(
                    &mut container,
                    &mut timing_metrics,
                    &mut count_metrics,
                    &decision,
                );
                assert!(res.unwrap() == 2 * num_txs && !container.is_empty());
                black_box(container);
            },
        )
    });
}

criterion_group!(
    benches,
    //bench_transaction_creation,
    //bench_transaction_serialization
    bench_sanitized_transaction_receive_and_buffer2
);
criterion_main!(benches);
