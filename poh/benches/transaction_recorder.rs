use {
    agave_votor_messages::migration::MigrationStatus,
    criterion::{criterion_group, criterion_main, Criterion},
    crossbeam_channel::bounded,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_ledger::{
        blockstore::Blockstore, genesis_utils::create_genesis_config,
        get_tmp_ledger_path_auto_delete, leader_schedule_cache::LeaderScheduleCache,
    },
    solana_poh::{
        poh_controller::PohController,
        poh_recorder::PohRecorder,
        poh_service::{PohService, DEFAULT_HASHES_PER_BATCH, DEFAULT_PINNED_CPU_CORE},
        record_channels::record_channels,
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
    let (poh_recorder, _entry_receiver) = PohRecorder::new(
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

    let (record_sender, record_receiver) = record_channels(false);
    let transaction_recorder = TransactionRecorder::new(record_sender);

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

    let (mut poh_controller, poh_service_receiver) = PohController::new();
    let poh_recorder = Arc::new(RwLock::new(poh_recorder));
    let (record_receiver_sender, _record_receiver_receiver) = bounded(1);
    let poh_service = PohService::new(
        poh_recorder.clone(),
        &genesis_config_info.genesis_config.poh_config,
        exit.clone(),
        bank.ticks_per_slot(),
        DEFAULT_PINNED_CPU_CORE,
        DEFAULT_HASHES_PER_BATCH,
        record_receiver,
        poh_service_receiver,
        Arc::new(MigrationStatus::default()),
        record_receiver_sender,
    );
    poh_controller
        .set_bank_sync(BankWithScheduler::new_without_scheduler(bank.clone()))
        .unwrap();

    let mut group = c.benchmark_group("record_transactions");
    group.throughput(criterion::Throughput::Elements(
        NUM_TRANSACTIONS * NUM_BATCHES,
    ));
    group.bench_function("record_transactions", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;

            for _ in 0..iters {
                let tx_batches: Vec<_> = (0..NUM_BATCHES).map(|_| txs.clone()).collect();
                let next_slot = bank.slot().wrapping_add(1);
                poh_controller
                    .reset_sync(bank.clone(), Some((next_slot, next_slot)))
                    .unwrap();
                bank = Arc::new(Bank::new_from_parent(
                    bank.clone(),
                    &Pubkey::default(),
                    next_slot,
                ));
                poh_controller
                    .set_bank_sync(BankWithScheduler::new_without_scheduler(bank.clone()))
                    .unwrap();

                let start = Instant::now();
                for txs in tx_batches {
                    let summary = transaction_recorder.record_transactions(bank.bank_id(), txs);
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
