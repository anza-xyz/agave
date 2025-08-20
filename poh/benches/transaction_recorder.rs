use {
    criterion::{criterion_group, criterion_main, Criterion},
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_ledger::{
        blockstore::Blockstore, genesis_utils::create_genesis_config,
        get_tmp_ledger_path_auto_delete, leader_schedule_cache::LeaderScheduleCache,
    },
    solana_poh::{
        poh_recorder::PohRecorder,
        poh_service::{PohService, DEFAULT_HASHES_PER_BATCH, DEFAULT_PINNED_CPU_CORE},
        transaction_recorder::TransactionRecorder,
    },
    solana_poh_config::PohConfig,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, installed_scheduler_pool::BankWithScheduler},
    solana_transaction::versioned::VersionedTransaction,
    std::{
        sync::{atomic::AtomicBool, Arc, RwLock},
        time::{Duration, Instant},
    },
};

fn bench_record_transactions(c: &mut Criterion) {
    // Relatively small non-zero number of transactions to use for the benchmark.
    // Ensures we spend some time doing the hash work for entry recording.
    const NUM_TRANSACTIONS: u64 = 16;
    // Number of batches to process in each benchmark iteration.
    // Each batch contains NUM_TRANSACTIONS transactions.
    // This emulates a burst of work coming in from the scheduler.
    const NUM_BATCHES: u64 = 32;

    // Setup the PohService.
    let mut genesis_config_info = create_genesis_config(2);
    genesis_config_info.genesis_config.ticks_per_slot = solana_clock::DEFAULT_TICKS_PER_SLOT;
    genesis_config_info.genesis_config.poh_config = PohConfig {
        target_tick_duration: Duration::from_micros(
            solana_clock::DEFAULT_MS_PER_SLOT * 1_000 / solana_clock::DEFAULT_TICKS_PER_SLOT,
        ),
        target_tick_count: None,
        hashes_per_tick: Some(solana_clock::DEFAULT_HASHES_PER_TICK),
    };
    let exit = Arc::new(AtomicBool::new(false));
    let mut bank = Arc::new(Bank::new_for_tests(&genesis_config_info.genesis_config));
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Arc::new(
        Blockstore::open(ledger_path.path()).expect("Expected to be able to open database ledger"),
    );
    let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
        bank.tick_height(),
        bank.last_blockhash(),
        bank.clone(),
        None,
        bank.ticks_per_slot(),
        blockstore,
        &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
        &genesis_config_info.genesis_config.poh_config,
        exit.clone(),
    );
    poh_recorder.set_bank(BankWithScheduler::new_without_scheduler(bank.clone()));

    let (record_sender, record_receiver) = crossbeam_channel::unbounded();
    let transaction_recorder = TransactionRecorder::new(record_sender, exit.clone());

    let txs: Vec<_> = (0..NUM_TRANSACTIONS)
        .map(|_| {
            VersionedTransaction::from(solana_system_transaction::transfer(
                &Keypair::new(),
                &Pubkey::new_unique(),
                1,
                Hash::new_unique(),
            ))
        })
        .collect();

    let poh_recorder = Arc::new(RwLock::new(poh_recorder));
    let poh_service = PohService::new(
        poh_recorder.clone(),
        &genesis_config_info.genesis_config.poh_config,
        exit.clone(),
        bank.ticks_per_slot(),
        DEFAULT_PINNED_CPU_CORE,
        DEFAULT_HASHES_PER_BATCH,
        record_receiver,
    );

    let mut group = c.benchmark_group("record_transactions");
    group.throughput(criterion::Throughput::Elements(
        NUM_TRANSACTIONS * NUM_BATCHES,
    ));
    group.bench_function("record_transactions", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;

            for _ in 0..iters {
                let tx_batches: Vec<_> = (0..NUM_BATCHES).map(|_| txs.clone()).collect();
                poh_recorder.write().unwrap().clear_bank_for_test();
                bank = Arc::new(Bank::new_from_parent(
                    bank.clone(),
                    &Pubkey::default(),
                    bank.slot().wrapping_add(1),
                ));
                poh_recorder
                    .write()
                    .unwrap()
                    .set_bank(BankWithScheduler::new_without_scheduler(bank.clone()));

                let start = Instant::now();
                for txs in tx_batches {
                    let summary = transaction_recorder.record_transactions(bank.slot(), txs);
                    assert!(summary.result.is_ok());
                }
                let elapsed = start.elapsed();
                total = total.saturating_add(elapsed);
            }

            total
        })
    });

    exit.store(true, std::sync::atomic::Ordering::Relaxed);
    poh_service.join().unwrap();
}

criterion_group!(benches, bench_record_transactions,);
criterion_main!(benches);
