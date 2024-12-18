#![feature(test)]

extern crate test;

use {
    criterion::{criterion_group, criterion_main, BenchmarkId, Criterion},
    rand::{rngs::SmallRng, seq::SliceRandom, SeedableRng},
    solana_accounts_db::{
        accounts_db::AccountsDb, read_only_accounts_cache::ReadOnlyAccountsCache,
    },
    solana_sdk::account::{Account, ReadableAccount},
    std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread::Builder,
    },
};

const NUM_READERS_WRITERS: &[usize] = &[
    8,
    16,
    // These parameters are likely to freeze your computer, if it has less than
    // 32 cores.
    // 32, 64, 128
];
const EVICT_SAMPLE_SIZES: &[usize] = &[1, 2, 4, 8, 10, 16, 32];

/// Benchmarks the read-only cache eviction mechanism. It does so by performing
/// multithreaded reads and writes on a full cache. Each write triggers
/// eviction. Background reads add more contention.
fn bench_cache_eviction(c: &mut Criterion) {
    /// Number of 1 MiB accounts needed to initially fill the cache.
    const NUM_ACCOUNTS_INIT: usize = 410;
    /// Number of accounts used in the benchmarked writes (per thread).
    const NUM_NEW_ACCOUNTS_PER_THREAD: usize = 512;

    let mut group = c.benchmark_group("cache_eviction");

    for evict_sample_size in EVICT_SAMPLE_SIZES {
        for num_readers_writers in NUM_READERS_WRITERS {
            let cache = Arc::new(ReadOnlyAccountsCache::new(
                AccountsDb::DEFAULT_MAX_READ_ONLY_CACHE_DATA_SIZE_LO,
                AccountsDb::DEFAULT_MAX_READ_ONLY_CACHE_DATA_SIZE_HI,
                *evict_sample_size,
                AccountsDb::READ_ONLY_CACHE_MS_TO_SKIP_LRU_UPDATE,
            ));

            // Prepare accounts for the cache fillup.
            let pubkeys: Vec<_> = std::iter::repeat_with(solana_sdk::pubkey::new_rand)
                .take(NUM_ACCOUNTS_INIT)
                .collect();
            let accounts_data = std::iter::repeat(
                Account {
                    lamports: 1,
                    // 1 MiB
                    data: vec![1; 1024 * 1024],
                    ..Default::default()
                }
                .to_account_shared_data(),
            )
            .take(NUM_ACCOUNTS_INIT);
            let storable_accounts = pubkeys.iter().zip(accounts_data);

            // Fill up the cache.
            let slot = 0;
            for (pubkey, account) in storable_accounts {
                cache.store(*pubkey, slot, account);
            }

            // Prepare accounts for the new writes.
            let new_storable_accounts = std::iter::repeat_with(solana_sdk::pubkey::new_rand)
                .map(|pubkey| {
                    (
                        pubkey,
                        Account {
                            lamports: 1,
                            // 1 MiB
                            data: vec![1; 1024 * 1024],
                            ..Default::default()
                        }
                        .to_account_shared_data(),
                    )
                })
                .take(NUM_NEW_ACCOUNTS_PER_THREAD);

            // Spawn the reader threads in the background.
            let stop_reader = Arc::new(AtomicBool::new(false));
            let reader_handles = (0..*num_readers_writers).map(|i| {
                let cache = cache.clone();
                let pubkeys = pubkeys.clone();
                let stop_reader = stop_reader.clone();
                Builder::new()
                    .name(format!("reader{i:02}"))
                    .spawn({
                        move || {
                            // Continuously read random accounts.
                            let mut rng = SmallRng::seed_from_u64(i as u64);
                            while !stop_reader.load(Ordering::Relaxed) {
                                let pubkey = pubkeys.choose(&mut rng).unwrap();
                                test::black_box(cache.load(*pubkey, slot));
                            }
                        }
                    })
                    .unwrap()
            });

            // Benchmark reads and writes on a full cache, trigerring eviction on each
            // write.
            let slot = 1;
            group.sample_size(10);
            group.bench_function(
                BenchmarkId::new(
                    format!("cache_eviction_k_{evict_sample_size}"),
                    num_readers_writers,
                ),
                |b| {
                    b.iter(|| {
                        // Perform the writes.
                        let writer_handles = (0..*num_readers_writers).map(|i| {
                            let cache = cache.clone();
                            let new_storable_accounts = new_storable_accounts.clone();
                            Builder::new()
                                .name(format!("writer{i:02}"))
                                .spawn({
                                    move || {
                                        for (pubkey, account) in new_storable_accounts {
                                            cache.store(pubkey, slot, account);
                                        }
                                    }
                                })
                                .unwrap()
                        });

                        for writer_handle in writer_handles {
                            writer_handle.join().unwrap();
                        }
                    })
                },
            );

            stop_reader.store(true, Ordering::Relaxed);
            for reader_handle in reader_handles {
                reader_handle.join().unwrap();
            }
        }
    }
}

criterion_group!(benches, bench_cache_eviction);
criterion_main!(benches);
