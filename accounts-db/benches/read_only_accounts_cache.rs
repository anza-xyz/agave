use {
    criterion::{criterion_group, criterion_main, BenchmarkId, Criterion},
    rand::{rngs::SmallRng, seq::IndexedRandom as _, SeedableRng},
    solana_accounts_db::{
        accounts_db::AccountsDb, read_only_accounts_cache::ReadOnlyAccountsCache,
    },
    std::{
        hint::black_box,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread::Builder,
        time::{Duration, Instant},
    },
};
mod utils;

#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

/// Sizes of accounts.
///
/// - No data.
/// - 165 bytes (a token account).
/// - 200 bytes (a stake account).
/// - 300 kibibytes (an account with data).
const DATA_SIZES: &[usize] = &[0, 165, 200, 300 * 1024];
/// Distribution of the account sizes:
///
/// - 4% of accounts have no data.
/// - 75% of accounts are 165 bytes (a token account).
/// - 20% of accounts are 200 bytes (a stake account).
/// - 1% of accounts are 300 kibibytes (an account with data).
const WEIGHTS: &[usize] = &[4, 75, 20, 1];
/// Numbers of reader and writer threads to bench.
const NUM_READERS_WRITERS: &[usize] = &[
    8, 16,
    // These parameters are likely to freeze your computer, if it has less than
    // 32 cores.
    32, 64,
];

/// Benchmarks read-only cache loads and stores without causing eviction.
fn bench_read_only_accounts_cache(c: &mut Criterion) {
    let mut group = c.benchmark_group("read_only_accounts_cache");
    let slot = 0;

    // Prepare initial accounts, but make sure to not fill up the cache.
    let accounts: Vec<_> = utils::accounts_with_size_limit(
        255,
        DATA_SIZES,
        WEIGHTS,
        AccountsDb::DEFAULT_MAX_READ_ONLY_CACHE_DATA_SIZE_LO / 2,
    )
    .collect();
    let pubkeys: Vec<_> = accounts
        .iter()
        .map(|(pubkey, _)| pubkey.to_owned())
        .collect();

    for num_readers_writers in NUM_READERS_WRITERS {
        let cache = Arc::new(ReadOnlyAccountsCache::new(
            AccountsDb::DEFAULT_MAX_READ_ONLY_CACHE_DATA_SIZE_LO,
            AccountsDb::DEFAULT_MAX_READ_ONLY_CACHE_DATA_SIZE_HI,
            AccountsDb::DEFAULT_READ_ONLY_CACHE_EVICT_SAMPLE_SIZE,
        ));

        for (pubkey, account) in accounts.iter() {
            cache.store(*pubkey, slot, account.clone());
        }

        // Spawn the reader threads in the background. They are reading the
        // reading the initially inserted accounts.
        let stop_threads = Arc::new(AtomicBool::new(false));
        let reader_handles = (0..*num_readers_writers)
            .map(|i| {
                let stop_threads = Arc::clone(&stop_threads);
                let cache = Arc::clone(&cache);
                let pubkeys = pubkeys.clone();

                Builder::new()
                    .name(format!("reader{i:02}"))
                    .spawn({
                        move || {
                            // Continuously read random accounts.
                            let mut rng = SmallRng::seed_from_u64(i as u64);
                            while !stop_threads.load(Ordering::Relaxed) {
                                let pubkey = pubkeys.choose(&mut rng).unwrap();
                                black_box(cache.load(*pubkey, slot));
                            }
                        }
                    })
                    .unwrap()
            })
            .collect::<Vec<_>>();

        // Spawn the writer threads in the background.
        let slot = 1;
        let writer_handles = (0..*num_readers_writers)
            .map(|i| {
                let stop_threads = Arc::clone(&stop_threads);
                let cache = Arc::clone(&cache);
                let accounts = accounts.clone();

                Builder::new()
                    .name(format!("writer{i:02}"))
                    .spawn({
                        move || {
                            // Continuously write to already existing pubkeys.
                            let mut rng = SmallRng::seed_from_u64(100_u64.saturating_add(i as u64));
                            while !stop_threads.load(Ordering::Relaxed) {
                                let (pubkey, account) = accounts.choose(&mut rng).unwrap();
                                cache.store(*pubkey, slot, account.clone());
                            }
                        }
                    })
                    .unwrap()
            })
            .collect::<Vec<_>>();

        group.bench_function(BenchmarkId::new("store", num_readers_writers), |b| {
            b.iter_custom(|iters| {
                let mut total_time = Duration::new(0, 0);

                for (pubkey, account) in accounts.iter().cycle().take(iters as usize) {
                    // Measure only stores.
                    let start = Instant::now();
                    cache.store(*pubkey, slot, account.clone());
                    total_time = total_time.saturating_add(start.elapsed());
                }
                total_time
            })
        });
        group.bench_function(BenchmarkId::new("load", num_readers_writers), |b| {
            b.iter_custom(|iters| {
                let start = Instant::now();
                for (pubkey, _) in accounts.iter().cycle().take(iters as usize) {
                    black_box(cache.load(*pubkey, slot));
                }

                start.elapsed()
            })
        });

        stop_threads.store(true, Ordering::Relaxed);
        for reader_handle in reader_handles {
            reader_handle.join().unwrap();
        }
        for writer_handle in writer_handles {
            writer_handle.join().unwrap();
        }
    }
}

/// Benchmarks the read-only cache eviction mechanism. It does so by performing
/// multithreaded reads and writes on a full cache. Each write triggers
/// eviction. Background reads add more contention.
fn bench_read_only_accounts_cache_eviction(
    c: &mut Criterion,
    group_name: &str,
    max_data_size_lo: usize,
    max_data_size_hi: usize,
) {
    // Prepare initial accounts, two times the high limit of the cache, to make
    // sure that the background threads sometimes try to store something which
    // is not in the cache.
    let accounts: Vec<_> = utils::accounts_with_size_limit(
        255,
        DATA_SIZES,
        WEIGHTS,
        AccountsDb::DEFAULT_MAX_READ_ONLY_CACHE_DATA_SIZE_HI * 2,
    )
    .collect();
    let pubkeys: Vec<_> = accounts
        .iter()
        .map(|(pubkey, _)| pubkey.to_owned())
        .collect();

    let mut group = c.benchmark_group(group_name);

    for num_readers_writers in NUM_READERS_WRITERS {
        let cache = Arc::new(ReadOnlyAccountsCache::new(
            max_data_size_lo,
            max_data_size_hi,
            AccountsDb::DEFAULT_READ_ONLY_CACHE_EVICT_SAMPLE_SIZE,
        ));

        // Fill up the cache.
        let slot = 0;
        for (pubkey, account) in accounts.iter() {
            cache.store(*pubkey, slot, account.clone());
        }

        // Spawn the reader threads in the background. They are reading the
        // reading the initially inserted accounts.
        let stop_threads = Arc::new(AtomicBool::new(false));
        let reader_handles = (0..*num_readers_writers)
            .map(|i| {
                let stop_threads = Arc::clone(&stop_threads);
                let cache = Arc::clone(&cache);
                let pubkeys = pubkeys.clone();

                Builder::new()
                    .name(format!("reader{i:02}"))
                    .spawn({
                        move || {
                            // Continuously read random accounts.
                            let mut rng = SmallRng::seed_from_u64(i as u64);
                            while !stop_threads.load(Ordering::Relaxed) {
                                let pubkey = pubkeys.choose(&mut rng).unwrap();
                                black_box(cache.load(*pubkey, slot));
                            }
                        }
                    })
                    .unwrap()
            })
            .collect::<Vec<_>>();

        // Spawn the writer threads in the background. Prepare the accounts
        // with the same public keys and sizes as the initial ones. The
        // intention is a constant overwrite in background for additional
        // contention.
        let slot = 1;
        let writer_handles = (0..*num_readers_writers)
            .map(|i| {
                let stop_threads = Arc::clone(&stop_threads);
                let cache = Arc::clone(&cache);
                let accounts = accounts.clone();

                Builder::new()
                    .name(format!("writer{i:02}"))
                    .spawn({
                        move || {
                            // Continuously write to already existing pubkeys.
                            let mut rng = SmallRng::seed_from_u64(100_u64.saturating_add(i as u64));
                            while !stop_threads.load(Ordering::Relaxed) {
                                let (pubkey, account) = accounts.choose(&mut rng).unwrap();
                                cache.store(*pubkey, slot, account.clone());
                            }
                        }
                    })
                    .unwrap()
            })
            .collect::<Vec<_>>();

        // Benchmark the performance of loading and storing accounts in a
        // cache that is fully populated. This triggers eviction for each
        // write operation. Background threads introduce contention.
        group.bench_function(BenchmarkId::new("load", num_readers_writers), |b| {
            b.iter_custom(|iters| {
                let mut rng = SmallRng::seed_from_u64(1);
                let mut total_time = Duration::new(0, 0);

                for _ in 0..iters {
                    let pubkey = pubkeys.choose(&mut rng).unwrap().to_owned();

                    let start = Instant::now();
                    black_box(cache.load(pubkey, slot));
                    total_time = total_time.saturating_add(start.elapsed());
                }

                total_time
            })
        });
        group.bench_function(BenchmarkId::new("store", num_readers_writers), |b| {
            b.iter_custom(|iters| {
                let accounts = utils::accounts(0, DATA_SIZES, WEIGHTS).take(iters as usize);

                let start = Instant::now();
                for (pubkey, account) in accounts {
                    cache.store(pubkey, slot, account);
                }

                start.elapsed()
            })
        });

        stop_threads.store(true, Ordering::Relaxed);
        for reader_handle in reader_handles {
            reader_handle.join().unwrap();
        }
        for writer_handle in writer_handles {
            writer_handle.join().unwrap();
        }
    }
}

/// Benchmarks read-only cache eviction with low and high thresholds. After
/// each eviction, enough stores need to be made to reach the difference
/// between the low and high threshold, triggering another eviction.
///
/// Even though eviction is not made on each store, the number of iterations
/// are high enough to trigger eviction often. Contention which comes from
/// locking the cache is still visible both in the benchmark's time and
/// profiles gathered from the benchmark run.
///
/// This benchmark aims to simulate contention in a manner close to what occurs
/// on validators.
fn bench_read_only_accounts_cache_eviction_lo_hi(c: &mut Criterion) {
    bench_read_only_accounts_cache_eviction(
        c,
        "read_only_accounts_cache_eviction_lo_hi",
        AccountsDb::DEFAULT_MAX_READ_ONLY_CACHE_DATA_SIZE_LO,
        AccountsDb::DEFAULT_MAX_READ_ONLY_CACHE_DATA_SIZE_HI,
    )
}

/// Benchmarks read-only cache eviction without differentiating between low and
/// high thresholds. Each store triggers another eviction immediately.
///
/// This benchmark measures the absolutely worst-case scenario, which may not
/// reflect actual conditions in validators.
fn bench_read_only_accounts_cache_eviction_hi(c: &mut Criterion) {
    bench_read_only_accounts_cache_eviction(
        c,
        "read_only_accounts_cache_eviction_hi",
        AccountsDb::DEFAULT_MAX_READ_ONLY_CACHE_DATA_SIZE_HI,
        AccountsDb::DEFAULT_MAX_READ_ONLY_CACHE_DATA_SIZE_HI,
    )
}

criterion_group!(
    benches,
    bench_read_only_accounts_cache,
    bench_read_only_accounts_cache_eviction_lo_hi,
    bench_read_only_accounts_cache_eviction_hi
);
criterion_main!(benches);
