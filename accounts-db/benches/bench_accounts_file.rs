#![allow(clippy::arithmetic_side_effects)]
use {
    criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput},
    solana_account::{AccountSharedData, ReadableAccount},
    solana_accounts_db::{
        accounts_file::StorageAccess,
        append_vec::{self, AppendVec},
        tiered_storage::{
            file::TieredReadableFile,
            hot::{HotStorageReader, HotStorageWriter, RENT_EXEMPT_RENT_EPOCH},
        },
        utils::create_account_shared_data,
    },
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    solana_system_interface::MAX_PERMITTED_DATA_LENGTH,
    std::mem::ManuallyDrop,
};

mod utils;

#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

const ACCOUNTS_COUNTS: [usize; 4] = [
    1,      // the smallest count; will bench overhead
    100,    // lower range of accounts written per slot on mnb
    1_000,  // higher range of accounts written per slot on mnb
    10_000, // reasonable largest number of accounts written per slot
];

fn bench_write_accounts_file(c: &mut Criterion, storage_access: StorageAccess) {
    let mut group = c.benchmark_group(format!("write_accounts_file_{storage_access:?}"));

    // most accounts on mnb are 165-200 bytes, so use that here too
    let space = 200;
    let lamports = 2_282_880; // the rent-exempt amount for 200 bytes of data
    let temp_dir = tempfile::tempdir().unwrap();

    for accounts_count in ACCOUNTS_COUNTS {
        group.throughput(Throughput::Elements(accounts_count as u64));

        let accounts: Vec<_> = std::iter::repeat_with(|| {
            (
                Pubkey::new_unique(),
                AccountSharedData::new_rent_epoch(
                    lamports,
                    space,
                    &Pubkey::new_unique(),
                    RENT_EXEMPT_RENT_EPOCH,
                ),
            )
        })
        .take(accounts_count)
        .collect();
        let accounts_refs: Vec<_> = accounts
            .iter()
            .map(|(pubkey, account)| (pubkey, account))
            .collect();
        let storable_accounts = (Slot::MAX, accounts_refs.as_slice());

        group.bench_function(BenchmarkId::new("append_vec", accounts_count), |b| {
            b.iter_batched_ref(
                || {
                    let path = temp_dir.path().join(format!("append_vec_{accounts_count}"));
                    let file_size = accounts.len() * (space + append_vec::STORE_META_OVERHEAD);
                    AppendVec::new(path, true, file_size, storage_access)
                },
                |append_vec| {
                    let res = append_vec.append_accounts(&storable_accounts, 0).unwrap();
                    let accounts_written_count = res.offsets.len();
                    assert_eq!(accounts_written_count, accounts_count);
                },
                BatchSize::SmallInput,
            );
        });

        group.bench_function(BenchmarkId::new("hot_storage", accounts_count), |b| {
            b.iter_batched_ref(
                || {
                    let path = temp_dir
                        .path()
                        .join(format!("hot_storage_{accounts_count}"));
                    _ = std::fs::remove_file(&path);
                    HotStorageWriter::new(path).unwrap()
                },
                |hot_storage| {
                    let res = hot_storage.write_accounts(&storable_accounts, 0).unwrap();
                    let accounts_written_count = res.offsets.len();
                    assert_eq!(accounts_written_count, accounts_count);
                    // Purposely do not call hot_storage.flush() here, since it will impact the
                    // bench.  Flushing will be handled by Drop, which is *not* timed (and that's
                    // what we want).
                },
                BatchSize::SmallInput,
            );
        });
    }
}

fn bench_write_accounts_file_file_io(c: &mut Criterion) {
    bench_write_accounts_file(c, StorageAccess::File);
}

fn bench_write_accounts_file_mmap(c: &mut Criterion) {
    bench_write_accounts_file(
        c,
        #[allow(deprecated)]
        StorageAccess::Mmap,
    );
}

fn bench_scan_pubkeys(c: &mut Criterion) {
    let mut group = c.benchmark_group("scan_pubkeys");
    let temp_dir = tempfile::tempdir().unwrap();

    // distribution of account data sizes to use when creating accounts
    // 3% of accounts have no data
    // 75% of accounts are 165 bytes (a token account)
    // 20% of accounts are 200 bytes (a stake account)
    // 1% of accounts are 64 kibibytes (pathological case for the scan buffer)
    // 1% of accounts are 10 mebibytes (the max size for an account)
    let data_sizes = [0, 165, 200, 1 << 16, MAX_PERMITTED_DATA_LENGTH as usize];
    let weights = [3, 75, 20, 1, 1];

    for accounts_count in ACCOUNTS_COUNTS {
        group.throughput(Throughput::Elements(accounts_count as u64));

        let storable_accounts: Vec<_> = utils::accounts(255, &data_sizes, &weights)
            .take(accounts_count)
            .collect();

        // create an append vec file
        let append_vec_path = temp_dir.path().join(format!("append_vec_{accounts_count}"));
        _ = std::fs::remove_file(&append_vec_path);
        let file_size = storable_accounts
            .iter()
            .map(|(_, account)| append_vec::aligned_stored_size(account.data().len()))
            .sum();
        let append_vec = AppendVec::new(append_vec_path, true, file_size, StorageAccess::File);
        let stored_accounts_info = append_vec
            .append_accounts(&(Slot::MAX, storable_accounts.as_slice()), 0)
            .unwrap();
        assert_eq!(stored_accounts_info.offsets.len(), accounts_count);
        append_vec.flush().unwrap();
        // Open append vecs for reading here, outside of the bench function, so we don't open lots
        // of file handles and run out/crash.  We also need to *not* remove the backing file in
        // these new append vecs because that would cause double-free (or triple-free here).
        // Wrap the append vecs in ManuallyDrop to *not* remove the backing file on drop.
        let append_vec_mmap = ManuallyDrop::new(
            AppendVec::new_from_file(
                append_vec.path(),
                append_vec.len(),
                #[allow(deprecated)]
                StorageAccess::Mmap,
            )
            .unwrap()
            .0,
        );
        let append_vec_file = ManuallyDrop::new(
            AppendVec::new_from_file(append_vec.path(), append_vec.len(), StorageAccess::File)
                .unwrap()
                .0,
        );

        // create a hot storage file
        let hot_storage_path = temp_dir
            .path()
            .join(format!("hot_storage_{accounts_count}"));
        _ = std::fs::remove_file(&hot_storage_path);
        let mut hot_storage_writer = HotStorageWriter::new(&hot_storage_path).unwrap();
        let stored_accounts_info = hot_storage_writer
            .write_accounts(&(Slot::MAX, storable_accounts.as_slice()), 0)
            .unwrap();
        assert_eq!(stored_accounts_info.offsets.len(), accounts_count);
        hot_storage_writer.flush().unwrap();
        // Similar to the append vec case above, open the hot storage for reading here.
        let hot_storage_file = TieredReadableFile::new(&hot_storage_path).unwrap();
        let hot_storage_reader = HotStorageReader::new(hot_storage_file).unwrap();

        group.bench_function(BenchmarkId::new("append_vec_mmap", accounts_count), |b| {
            b.iter(|| {
                let mut count = 0;
                append_vec_mmap.scan_pubkeys(|_| count += 1).unwrap();
                assert_eq!(count, accounts_count);
            });
        });
        group.bench_function(BenchmarkId::new("append_vec_file", accounts_count), |b| {
            b.iter(|| {
                let mut count = 0;
                append_vec_file.scan_pubkeys(|_| count += 1).unwrap();
                assert_eq!(count, accounts_count);
            });
        });
        group.bench_function(BenchmarkId::new("hot_storage", accounts_count), |b| {
            b.iter(|| {
                let mut count = 0;
                hot_storage_reader.scan_pubkeys(|_| count += 1).unwrap();
                assert_eq!(count, accounts_count);
            });
        });
    }
}

// AppendVec file io has a custom impl for `get_account_shared_data()` that avoids an extra
// allocation when the account data exceeds the stack buffer.
// This benchmark times how beneficial this custom impl actually is.  IOW, if the custom impl takes
// the same time as the `get_stored_account_callback().to_account_shared_data()` impl, then we can
// remove the custom impl.
fn bench_get_account_shared_data(c: &mut Criterion) {
    let mut group = c.benchmark_group("get_account_shared_data");
    let temp_dir = tempfile::tempdir().unwrap();

    const DATA_SIZES: [usize; 4] = [
        200,                                /* small data, *does* fit in stack buffer */
        4 * 1024,                           /* medium data, does *not* fit in stack buffer */
        1_000_000,                          /* large data, does *not* fix in stack buffer */
        MAX_PERMITTED_DATA_LENGTH as usize, /* max data, worst-case allocation */
    ];
    for data_size in DATA_SIZES {
        let storable_accounts: Vec<_> = utils::accounts(255, &[data_size], &[1]).take(1).collect();

        // create an append vec file
        let append_vec_path = temp_dir.path().join(format!("append_vec_{data_size}"));
        _ = std::fs::remove_file(&append_vec_path);
        let file_size = storable_accounts
            .iter()
            .map(|(_, account)| append_vec::aligned_stored_size(account.data().len()))
            .sum();
        let append_vec = AppendVec::new(append_vec_path, true, file_size, StorageAccess::File);
        let stored_accounts_info = append_vec
            .append_accounts(&(Slot::MAX, storable_accounts.as_slice()), 0)
            .unwrap();
        assert_eq!(stored_accounts_info.offsets.len(), 1);
        append_vec.flush().unwrap();
        // Open append vecs for reading here, outside of the bench function, so we don't open lots
        // of file handles and run out/crash.  We also need to *not* remove the backing file in
        // these new append vecs because that would cause double-free (or triple-free here).
        // Wrap the append vecs in ManuallyDrop to *not* remove the backing file on drop.
        let append_vec_mmap = ManuallyDrop::new(
            AppendVec::new_from_file(
                append_vec.path(),
                append_vec.len(),
                #[allow(deprecated)]
                StorageAccess::Mmap,
            )
            .unwrap()
            .0,
        );
        let append_vec_file = ManuallyDrop::new(
            AppendVec::new_from_file(append_vec.path(), append_vec.len(), StorageAccess::File)
                .unwrap()
                .0,
        );

        // Run the benchmarks!
        // Note, use `iter_with_large_drop()` to avoid timing how long it takes to drop the Vec of
        // account data.
        group.bench_function(
            // The baseline.
            BenchmarkId::new("append_vec_mmap_get_account_shared_data", data_size),
            |b| {
                b.iter_with_large_drop(|| {
                    _ = append_vec_mmap.get_account_shared_data(0).unwrap();
                });
            },
        );
        group.bench_function(
            // The mmap "baseline" impl (above) does exactly the same as this one (below),
            // so we expect perf to be identical.
            BenchmarkId::new("append_vec_mmap_get_stored_account_callback", data_size),
            |b| {
                b.iter_with_large_drop(|| {
                    _ = append_vec_mmap
                        .get_stored_account_callback(0, |account| {
                            create_account_shared_data(&account)
                        })
                        .unwrap();
                });
            },
        );
        group.bench_function(
            // The custom file io impl, which avoids the extra allocation.
            BenchmarkId::new("append_vec_file_get_account_shared_data", data_size),
            |b| {
                b.iter_with_large_drop(|| {
                    _ = append_vec_file.get_account_shared_data(0).unwrap();
                });
            },
        );
        group.bench_function(
            // The "default" file io impl, which requires an additional allocation.
            // For the larger data sizes that do not fit in the stack buffer, we expect this impl
            // to be slower than the custom impl (above).
            BenchmarkId::new("append_vec_file_get_stored_account_callback", data_size),
            |b| {
                b.iter_with_large_drop(|| {
                    _ = append_vec_file
                        .get_stored_account_callback(0, |account| {
                            create_account_shared_data(&account)
                        })
                        .unwrap();
                });
            },
        );
    }
}

criterion_group!(
    benches,
    bench_write_accounts_file_file_io,
    bench_write_accounts_file_mmap,
    bench_scan_pubkeys,
    bench_get_account_shared_data,
);
criterion_main!(benches);
