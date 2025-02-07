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

// So in theory one thread should be able to generate 10 batches per second. Which sounds to be enough.

fn bench_sanitized_transaction_receive_and_buffer(c: &mut Criterion) {
    let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(100_000);
    let (bank, bank_forks) =
        Bank::new_for_benches(&genesis_config).wrap_with_bank_forks_for_tests();
    let bank_start = BankStart {
        working_bank: bank,
        bank_creation_time: Arc::new(Instant::now()),
    };

    // TODO maybe use `let bank = Arc::new(Bank::default_for_tests());`?

    // Need to create all the accounts used in the transactions?
    // Need to create a thread that takes sender and these accounts to produce transactions
    // Do I need to properly fund these txs?

    let (sender, receiver) = unbounded();
    let handle = thread::spawn(move || {
        // Can we just generate random bytes?
        // prepare keypairs
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

        let packets_batches = BankingPacketBatch::new(to_packet_batches(&txs, NUM_PACKETS));
        //TODO Is it ok we put the same txs again? Do we check for duplicates on this stage?
        loop {
            if sender.send(packets_batches.clone()).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    });

    let mut rb = SanitizedTransactionReceiveAndBuffer::new(
        PacketDeserializer::new(receiver),
        bank_forks,
        false,
    );

    const TOTAL_BUFFERED_PACKETS: usize = 100_000;
    let mut container =
        <SanitizedTransactionReceiveAndBuffer as ReceiveAndBuffer>::Container::with_capacity(
            TOTAL_BUFFERED_PACKETS,
        );
    let mut count_metrics = SchedulerCountMetrics::default();
    let mut timing_metrics = SchedulerTimingMetrics::default();
    let decision = BufferedPacketsDecision::Consume(bank_start);

    // TODO use this construction instead to use always new container
    //  b.iter_batched(|| data.clone(), |mut data| sort(&mut data), BatchSize::SmallInput)
    c.bench_function("sanitized_transaction_receive_and_buffer", |bencher| {
        bencher.iter(|| {
            rb.receive_and_buffer_packets(
                &mut container,
                &mut timing_metrics,
                &mut count_metrics,
                &decision,
            );
            // clear to have the same situation on every iteration
            container.clear();
        })
    });
    drop(rb);
    assert!(handle.join().is_ok());
}

criterion_group!(
    benches,
    //bench_transaction_creation,
    //bench_transaction_serialization
    bench_sanitized_transaction_receive_and_buffer
);
criterion_main!(benches);
