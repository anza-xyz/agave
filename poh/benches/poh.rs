// This bench attempts to justify the value of `solana_poh::poh_service::DEFAULT_HASHES_PER_BATCH`.

use {
    bencher::{Bencher, benchmark_group, benchmark_main},
    solana_entry::poh::Poh,
    solana_hash::Hash,
    solana_ledger::{
        blockstore::Blockstore,
        genesis_utils::{GenesisConfigInfo, create_genesis_config},
        get_tmp_ledger_path_auto_delete,
        leader_schedule_cache::LeaderScheduleCache,
    },
    solana_perf::test_tx::test_tx,
    solana_poh::{poh_recorder::PohRecorder, poh_service::DEFAULT_HASHES_PER_BATCH},
    solana_poh_config::PohConfig,
    solana_runtime::bank::Bank,
    solana_sha256_hasher::hash,
    solana_transaction::sanitized::SanitizedTransaction,
    std::{
        hint::black_box,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
    },
};

#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

const NUM_HASHES: u64 = 30_000; // Should require ~10ms on a 2017 MacBook Pro

// No locking.  Fastest.
fn bench_poh_hash(b: &mut Bencher) {
    let mut poh = Poh::new(Hash::default(), None);
    b.iter(|| {
        poh.hash(NUM_HASHES);
    })
}

// Lock on each iteration.  Slowest.
fn bench_arc_mutex_poh_hash(b: &mut Bencher) {
    let poh = Arc::new(Mutex::new(Poh::new(Hash::default(), None)));
    b.iter(|| {
        for _ in 0..NUM_HASHES {
            poh.lock().unwrap().hash(1);
        }
    })
}

// Acquire lock every NUM_HASHES_PER_BATCH iterations.
// Speed should be close to bench_poh_hash() if NUM_HASHES_PER_BATCH is set well.
fn bench_arc_mutex_poh_batched_hash(b: &mut Bencher) {
    let poh = Arc::new(Mutex::new(Poh::new(Hash::default(), Some(NUM_HASHES))));
    //let exit = Arc::new(AtomicBool::new(false));
    let exit = Arc::new(AtomicBool::new(true));

    b.iter(|| {
        // NOTE: This block attempts to look as close as possible to `PohService::tick_producer()`
        loop {
            if poh.lock().unwrap().hash(DEFAULT_HASHES_PER_BATCH) {
                poh.lock().unwrap().tick().unwrap();
                if exit.load(Ordering::Relaxed) {
                    break;
                }
            }
        }
    })
}

// Worst case transaction record delay due to batch hashing at NUM_HASHES_PER_BATCH
fn bench_poh_lock_time_per_batch(b: &mut Bencher) {
    let mut poh = Poh::new(Hash::default(), None);
    b.iter(|| {
        poh.hash(DEFAULT_HASHES_PER_BATCH);
    })
}

fn bench_poh_recorder_record(b: &mut Bencher) {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore =
        Blockstore::open(ledger_path.path()).expect("Expected to be able to open database ledger");
    let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
    let bank = Arc::new(Bank::new_for_tests(&genesis_config));
    let prev_hash = bank.last_blockhash();

    let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
        0,
        prev_hash,
        bank.clone(),
        Some((4, 4)),
        bank.ticks_per_slot(),
        Arc::new(blockstore),
        &std::sync::Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
        &PohConfig::default(),
        Arc::new(AtomicBool::default()),
    );
    let h1 = hash(b"hello Agave, hello Anza!");

    poh_recorder.set_bank_for_test(bank.clone());
    poh_recorder.tick();
    let txs: [SanitizedTransaction; 7] = [
        SanitizedTransaction::from_transaction_for_tests(test_tx()),
        SanitizedTransaction::from_transaction_for_tests(test_tx()),
        SanitizedTransaction::from_transaction_for_tests(test_tx()),
        SanitizedTransaction::from_transaction_for_tests(test_tx()),
        SanitizedTransaction::from_transaction_for_tests(test_tx()),
        SanitizedTransaction::from_transaction_for_tests(test_tx()),
        SanitizedTransaction::from_transaction_for_tests(test_tx()),
    ];

    let txs: Vec<_> = txs.iter().map(|tx| tx.to_versioned_transaction()).collect();
    b.iter(|| {
        let _record_result = poh_recorder
            .record(
                bank.slot(),
                vec![black_box(h1)],
                vec![black_box(txs.clone())],
            )
            .unwrap();
    });
    poh_recorder.tick();
}

fn bench_poh_recorder_set_bank(b: &mut Bencher) {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore =
        Blockstore::open(ledger_path.path()).expect("Expected to be able to open database ledger");
    let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
    let bank = Arc::new(Bank::new_for_tests(&genesis_config));
    let prev_hash = bank.last_blockhash();

    let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
        0,
        prev_hash,
        bank.clone(),
        Some((4, 4)),
        bank.ticks_per_slot(),
        Arc::new(blockstore),
        &std::sync::Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
        &PohConfig::default(),
        Arc::new(AtomicBool::default()),
    );
    b.iter(|| {
        poh_recorder.set_bank_for_test(bank.clone());
        poh_recorder.tick();
        poh_recorder.clear_bank_for_test();
    });
}

benchmark_group!(
    benches,
    bench_arc_mutex_poh_batched_hash,
    bench_arc_mutex_poh_hash,
    bench_poh_hash,
    bench_poh_lock_time_per_batch,
    bench_poh_recorder_record,
    bench_poh_recorder_set_bank
);
benchmark_main!(benches);
