#![feature(test)]

extern crate test;

use {
    criterion::{criterion_group, criterion_main, BenchmarkId, Criterion},
    ndarray::{Array2, ArrayView},
    rand::{rngs::SmallRng, seq::SliceRandom, SeedableRng},
    solana_accounts_db::{
        accounts_db::AccountsDb, read_only_accounts_cache::ReadOnlyAccountsCache,
    },
    solana_sdk::{
        account::{Account, ReadableAccount},
        pubkey::Pubkey,
    },
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
    // 32, 64, 128, 256, 512, 1024,
];

/// Benchmarks the read-only cache eviction mechanism. It does so by performing
/// multithreaded reads and writes on a full cache. Each write triggers
/// eviction. Background reads add more contention.
fn bench_read_only_accounts_cache_eviction(c: &mut Criterion) {
    /// Number of 1 MiB accounts needed to initially fill the cache.
    const NUM_ACCOUNTS_INIT: usize = 410;
    /// Number of accounts used in the benchmarked writes (per thread).
    const NUM_ACCOUNTS_PER_THREAD: usize = 512;

    let mut group = c.benchmark_group("cache_eviction");

    for num_readers_writers in NUM_READERS_WRITERS {
        // Test on even numbers of threads.
        assert!(*num_readers_writers % 2 == 0);

        let cache = Arc::new(ReadOnlyAccountsCache::new(
            AccountsDb::DEFAULT_MAX_READ_ONLY_CACHE_DATA_SIZE_LO,
            AccountsDb::DEFAULT_MAX_READ_ONLY_CACHE_DATA_SIZE_HI,
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

        // Prepare accounts for the N write threads. We want to perform both
        // new writes and updates in each of them. In general, half of the
        // operations should be new writes, other half - updates.
        //
        // To achieve that, generate a 2D array of public keys, with N colums
        // and `NUM_ACCOUNTS_PER_THREAD` rows. Take the following steps:
        //
        // * Generate `NUM_ACCOUNTS_PER_THREAD / 2` rows with unique pubkeys.
        // * Add `NUM_ACCOUNTS_PER_THREAD / 2` rows, with the same pubkeys as
        //   the upper half, but shuffled across columns. Example:
        //   * Upper rows:
        //     [0, 1, 2, 3]
        //     [4, 5, 6, 7]
        //     [...]
        //   * Bottom rows:
        //     [2, 1, 3, 0]
        //     [5, 4, 7, 6]
        //     [...]
        // * That already gives us set of pubkeys where half is new and half
        //   triggers an update. But if we used the columns as they are right
        //   now, each thread would firstly write new accounts, and then
        //   update, these actiouns would be done in the same order.
        //   To add some entrophy here, shuffle the columns.
        let mut rng = SmallRng::seed_from_u64(100);
        let mut new_pubkeys: Array2<Pubkey> = Array2::from_shape_vec(
            (NUM_ACCOUNTS_PER_THREAD / 2, *num_readers_writers),
            vec![
                solana_sdk::pubkey::new_rand();
                *num_readers_writers * (NUM_ACCOUNTS_PER_THREAD / 2)
            ],
        )
        .unwrap();
        let new_rows: Vec<Vec<Pubkey>> = new_pubkeys
            .rows()
            .into_iter()
            .map(|row| {
                let mut shuffled_row = row.to_vec();
                shuffled_row.shuffle(&mut rng);
                shuffled_row
            })
            .collect();
        for new_row in new_rows {
            new_pubkeys
                .push_row(ArrayView::from(new_row.as_slice()))
                .unwrap();
        }
        let new_pubkeys: Vec<Vec<Pubkey>> = new_pubkeys
            .columns()
            .into_iter()
            .map(|column| {
                // Both `ArrayBase::as_slice` and `ArrayBase::as_mut_slice`
                // return `None` in this case, so let's just collect the elements.
                let mut pubkeys_for_thread = column
                    .into_iter()
                    .map(|pubkey| pubkey.to_owned())
                    .collect::<Vec<_>>();
                pubkeys_for_thread.shuffle(&mut rng);
                pubkeys_for_thread
            })
            .collect();

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
            BenchmarkId::new("read_only_accounts_cache_eviction", num_readers_writers),
            |b| {
                b.iter(|| {
                    // Perform the writes.
                    let writer_handles = (0..*num_readers_writers).map(|i| {
                        let cache = cache.clone();
                        let new_pubkeys = new_pubkeys[i].clone();

                        Builder::new()
                            .name(format!("writer{i:02}"))
                            .spawn({
                                move || {
                                    for pubkey in new_pubkeys {
                                        cache.store(
                                            pubkey,
                                            slot,
                                            Account {
                                                lamports: 1,
                                                // 1 MiB
                                                data: vec![1; 1024 * 1024],
                                                ..Default::default()
                                            }
                                            .to_account_shared_data(),
                                        );
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

criterion_group!(benches, bench_read_only_accounts_cache_eviction);
criterion_main!(benches);
