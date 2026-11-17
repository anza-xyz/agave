use {
    bencher::{Bencher, benchmark_group, benchmark_main},
    crossbeam_channel::{Receiver, Sender, bounded, select},
    solana_entry::poh::Poh,
    solana_hash::Hash,
    solana_ledger::{
        blockstore::Blockstore,
        genesis_utils::{GenesisConfigInfo, create_genesis_config},
        get_tmp_ledger_path_auto_delete,
        leader_schedule_cache::LeaderScheduleCache,
    },
    solana_perf::test_tx::test_tx,
    solana_poh::poh_recorder::PohRecorder,
    solana_poh_config::PohConfig,
    solana_runtime::bank::Bank,
    solana_sha256_hasher::hash,
    solana_transaction::sanitized::SanitizedTransaction,
    std::{
        hint::black_box,
        sync::{Arc, Mutex, atomic::AtomicBool},
        thread::{self, JoinHandle},
    },
};

#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

/// Reads and discards a channel's items until dropped.
///
/// Useful for a bounded channel whose sender blocks when full and has no other
/// reader.
struct EntryDrain {
    // Never sent on - the thread's select loop ends when this drops.
    keep_alive: Option<Sender<()>>,
    thread_hdl: Option<JoinHandle<()>>,
}

impl EntryDrain {
    fn new<T: Send + 'static>(receiver: Receiver<T>) -> Self {
        let (keep_alive, stop_receiver) = bounded::<()>(0);
        let thread_hdl = thread::spawn(move || {
            loop {
                select! {
                    recv(receiver) -> entry => if entry.is_err() { break },
                    recv(stop_receiver) -> _ => break,
                }
            }
        });
        Self {
            keep_alive: Some(keep_alive),
            thread_hdl: Some(thread_hdl),
        }
    }
}

impl Drop for EntryDrain {
    fn drop(&mut self) {
        self.keep_alive.take();
        if let Some(thread_hdl) = self.thread_hdl.take() {
            let _ = thread_hdl.join();
        }
    }
}

const NUM_HASHES: u64 = 30_000; // Should require ~10ms on a 2017 MacBook Pro

// No locking. Fastest.
fn bench_poh_hash(bencher: &mut Bencher) {
    let mut poh = Poh::new(Hash::default(), None);
    bencher.iter(|| {
        poh.hash(NUM_HASHES);
    })
}

// Lock on each iteration. Slowest.
fn bench_arc_mutex_poh_hash(bencher: &mut Bencher) {
    let poh = Arc::new(Mutex::new(Poh::new(Hash::default(), None)));
    bencher.iter(|| {
        for _ in 0..NUM_HASHES {
            poh.lock().unwrap().hash(1);
        }
    })
}

fn bench_poh_recorder_record(bencher: &mut Bencher) {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore =
        Blockstore::open(ledger_path.path()).expect("Expected to be able to open database ledger");
    let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
    let bank = Arc::new(Bank::new_for_tests(&genesis_config));
    let prev_hash = bank.last_blockhash();

    let (mut poh_recorder, entry_receiver) = PohRecorder::new(
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
    let _entry_drain = EntryDrain::new(entry_receiver);
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
    bencher.iter(|| {
        let _record_result = poh_recorder
            .record(bank.bank_id(), black_box(h1), black_box(txs.clone()))
            .unwrap();
    });
    poh_recorder.tick();
}

fn bench_poh_recorder_set_bank(bencher: &mut Bencher) {
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore =
        Blockstore::open(ledger_path.path()).expect("Expected to be able to open database ledger");
    let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(2);
    let bank = Arc::new(Bank::new_for_tests(&genesis_config));
    let prev_hash = bank.last_blockhash();

    let (mut poh_recorder, entry_receiver) = PohRecorder::new(
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
    let _entry_drain = EntryDrain::new(entry_receiver);
    bencher.iter(|| {
        poh_recorder.set_bank_for_test(bank.clone());
        poh_recorder.tick();
        poh_recorder.clear_bank_for_test();
    });
}

benchmark_group!(
    benches,
    bench_arc_mutex_poh_hash,
    bench_poh_hash,
    bench_poh_recorder_record,
    bench_poh_recorder_set_bank
);
benchmark_main!(benches);
