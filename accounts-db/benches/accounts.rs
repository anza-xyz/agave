#![feature(test)]
#![allow(clippy::arithmetic_side_effects)]

extern crate test;

use {
    dashmap::DashMap,
    rand::Rng,
    rayon::iter::{IntoParallelRefIterator, ParallelIterator},
    solana_account::{AccountSharedData, ReadableAccount},
    solana_accounts_db::{
        account_info::{AccountInfo, StorageLocation},
        accounts::{AccountAddressFilter, Accounts},
        accounts_db::{AccountFromStorage, AccountsDb, ACCOUNTS_DB_CONFIG_FOR_BENCHMARKS},
        accounts_index::ScanConfig,
        ancestors::Ancestors,
    },
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    std::{
        collections::{HashMap, HashSet},
        path::PathBuf,
        sync::{Arc, RwLock},
        thread::Builder,
    },
    test::Bencher,
};

#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

fn new_accounts_db(account_paths: Vec<PathBuf>) -> AccountsDb {
    AccountsDb::new_with_config(
        account_paths,
        ACCOUNTS_DB_CONFIG_FOR_BENCHMARKS,
        None,
        Arc::default(),
    )
}

#[bench]
fn bench_delete_dependencies(bencher: &mut Bencher) {
    agave_logger::setup();
    let accounts_db = new_accounts_db(vec![PathBuf::from("accounts_delete_deps")]);
    let accounts = Accounts::new(Arc::new(accounts_db));
    let mut old_pubkey = Pubkey::default();
    let zero_account = AccountSharedData::new(0, 0, AccountSharedData::default().owner());
    for i in 0..1000 {
        let pubkey = solana_pubkey::new_rand();
        let account = AccountSharedData::new(i + 1, 0, AccountSharedData::default().owner());
        accounts
            .accounts_db
            .store_for_tests((i, [(&pubkey, &account)].as_slice()));
        accounts
            .accounts_db
            .store_for_tests((i, [(&old_pubkey, &zero_account)].as_slice()));
        old_pubkey = pubkey;
        accounts.accounts_db.add_root_and_flush_write_cache(i);
    }
    bencher.iter(|| {
        accounts.accounts_db.clean_accounts_for_tests();
    });
}

fn store_accounts_with_possible_contention<F>(bench_name: &str, bencher: &mut Bencher, reader_f: F)
where
    F: Fn(&Accounts, &[Pubkey]) + Send + Copy + 'static,
{
    let num_readers = 5;
    let accounts_db = new_accounts_db(vec![PathBuf::from(
        std::env::var("FARF_DIR").unwrap_or_else(|_| "farf".to_string()),
    )
    .join(bench_name)]);
    let accounts = Arc::new(Accounts::new(Arc::new(accounts_db)));
    let num_keys = 1000;
    let slot = 0;

    let pubkeys: Vec<_> = std::iter::repeat_with(solana_pubkey::new_rand)
        .take(num_keys)
        .collect();
    let accounts_data: Vec<_> = std::iter::repeat_n(
        AccountSharedData::new(1, 0, &Pubkey::new_from_array([0u8; 32])),
        num_keys,
    )
    .collect();
    let storable_accounts: Vec<_> = pubkeys.iter().zip(accounts_data.iter()).collect();
    accounts.store_accounts_par((slot, storable_accounts.as_slice()), None);
    accounts.add_root(slot);
    accounts
        .accounts_db
        .flush_accounts_cache_slot_for_tests(slot);

    let pubkeys = Arc::new(pubkeys);
    for i in 0..num_readers {
        let accounts = accounts.clone();
        let pubkeys = pubkeys.clone();
        Builder::new()
            .name(format!("reader{i:02}"))
            .spawn(move || {
                reader_f(&accounts, &pubkeys);
            })
            .unwrap();
    }

    let num_new_keys = 1000;
    bencher.iter(|| {
        let new_pubkeys: Vec<_> = std::iter::repeat_with(solana_pubkey::new_rand)
            .take(num_new_keys)
            .collect();
        let new_storable_accounts: Vec<_> = new_pubkeys.iter().zip(accounts_data.iter()).collect();
        // Write to a different slot than the one being read from. Because
        // there's a new account pubkey being written to every time, will
        // compete for the accounts index lock on every store
        accounts.store_accounts_par((slot + 1, new_storable_accounts.as_slice()), None);
    });
}

#[bench]
fn bench_concurrent_read_write(bencher: &mut Bencher) {
    store_accounts_with_possible_contention(
        "concurrent_read_write",
        bencher,
        |accounts, pubkeys| {
            let mut rng = rand::rng();
            loop {
                let i = rng.random_range(0..pubkeys.len());
                test::black_box(
                    accounts
                        .load_without_fixed_root(&Ancestors::default(), &pubkeys[i])
                        .unwrap(),
                );
            }
        },
    )
}

#[bench]
fn bench_concurrent_scan_write(bencher: &mut Bencher) {
    store_accounts_with_possible_contention("concurrent_scan_write", bencher, |accounts, _| loop {
        test::black_box(
            accounts
                .load_by_program(
                    &Ancestors::default(),
                    0,
                    AccountSharedData::default().owner(),
                    &ScanConfig::default(),
                )
                .unwrap(),
        );
    })
}

#[bench]
#[ignore]
fn bench_dashmap_single_reader_with_n_writers(bencher: &mut Bencher) {
    let num_readers = 5;
    let num_keys = 10000;
    let map = Arc::new(DashMap::new());
    for i in 0..num_keys {
        map.insert(i, i);
    }
    for _ in 0..num_readers {
        let map = map.clone();
        Builder::new()
            .name("readers".to_string())
            .spawn(move || loop {
                test::black_box(map.entry(5).or_insert(2));
            })
            .unwrap();
    }
    bencher.iter(|| {
        for _ in 0..num_keys {
            test::black_box(map.get(&5).unwrap().value());
        }
    })
}

#[bench]
#[ignore]
fn bench_rwlock_hashmap_single_reader_with_n_writers(bencher: &mut Bencher) {
    let num_readers = 5;
    let num_keys = 10000;
    let map = Arc::new(RwLock::new(HashMap::new()));
    for i in 0..num_keys {
        map.write().unwrap().insert(i, i);
    }
    for _ in 0..num_readers {
        let map = map.clone();
        Builder::new()
            .name("readers".to_string())
            .spawn(move || loop {
                test::black_box(map.write().unwrap().get(&5));
            })
            .unwrap();
    }
    bencher.iter(|| {
        for _ in 0..num_keys {
            test::black_box(map.read().unwrap().get(&5));
        }
    })
}

fn setup_bench_dashmap_iter() -> (Arc<Accounts>, DashMap<Pubkey, (AccountSharedData, Hash)>) {
    let accounts_db = new_accounts_db(vec![PathBuf::from(
        std::env::var("FARF_DIR").unwrap_or_else(|_| "farf".to_string()),
    )
    .join("bench_dashmap_par_iter")]);
    let accounts = Arc::new(Accounts::new(Arc::new(accounts_db)));

    let dashmap = DashMap::new();
    let num_keys = std::env::var("NUM_BENCH_KEYS")
        .map(|num_keys| num_keys.parse::<usize>().unwrap())
        .unwrap_or_else(|_| 10000);
    for _ in 0..num_keys {
        dashmap.insert(
            Pubkey::new_unique(),
            (
                AccountSharedData::new(1, 0, AccountSharedData::default().owner()),
                Hash::new_unique(),
            ),
        );
    }

    (accounts, dashmap)
}

#[bench]
fn bench_dashmap_par_iter(bencher: &mut Bencher) {
    let (accounts, dashmap) = setup_bench_dashmap_iter();

    bencher.iter(|| {
        test::black_box(accounts.accounts_db.thread_pool_foreground.install(|| {
            dashmap
                .par_iter()
                .map(|cached_account| (*cached_account.key(), cached_account.value().1))
                .collect::<Vec<(Pubkey, Hash)>>()
        }));
    });
}

#[bench]
fn bench_dashmap_iter(bencher: &mut Bencher) {
    let (_accounts, dashmap) = setup_bench_dashmap_iter();

    bencher.iter(|| {
        test::black_box(
            dashmap
                .iter()
                .map(|cached_account| (*cached_account.key(), cached_account.value().1))
                .collect::<Vec<(Pubkey, Hash)>>(),
        );
    });
}

#[bench]
fn bench_load_largest_accounts(b: &mut Bencher) {
    let accounts_db = new_accounts_db(Vec::new());
    let accounts = Accounts::new(Arc::new(accounts_db));
    let mut rng = rand::rng();
    for _ in 0..10_000 {
        let lamports = rng.random();
        let pubkey = Pubkey::new_unique();
        let account = AccountSharedData::new(lamports, 0, &Pubkey::default());
        accounts
            .accounts_db
            .store_for_tests((0, [(&pubkey, &account)].as_slice()));
    }
    accounts.accounts_db.add_root_and_flush_write_cache(0);
    let ancestors = Ancestors::from(vec![0]);
    let bank_id = 0;
    b.iter(|| {
        accounts.load_largest_accounts(
            &ancestors,
            bank_id,
            20,
            &HashSet::new(),
            AccountAddressFilter::Exclude,
            false,
        )
    });
}

#[bench]
fn bench_sort_and_remove_dups(b: &mut Bencher) {
    fn generate_sample_account_from_storage(i: u8) -> AccountFromStorage {
        // offset has to be 8 byte aligned
        let offset = (i as usize) * std::mem::size_of::<u64>();
        AccountFromStorage {
            index_info: AccountInfo::new(StorageLocation::AppendVec(i as u32, offset), i == 0),
            data_len: i as u64,
            pubkey: Pubkey::new_from_array([i; 32]),
        }
    }

    use rand::prelude::*;
    let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(1234);
    let accounts: Vec<_> =
        std::iter::repeat_with(|| generate_sample_account_from_storage(rng.random::<u8>()))
            .take(1000)
            .collect();

    b.iter(|| AccountsDb::sort_and_remove_dups(&mut accounts.clone()));
}

#[bench]
fn bench_sort_and_remove_dups_no_dups(b: &mut Bencher) {
    fn generate_sample_account_from_storage(i: u8) -> AccountFromStorage {
        // offset has to be 8 byte aligned
        let offset = (i as usize) * std::mem::size_of::<u64>();
        AccountFromStorage {
            index_info: AccountInfo::new(StorageLocation::AppendVec(i as u32, offset), i == 0),
            data_len: i as u64,
            pubkey: Pubkey::new_unique(),
        }
    }

    use rand::prelude::*;

    let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(1234);
    let mut accounts: Vec<_> =
        std::iter::repeat_with(|| generate_sample_account_from_storage(rng.random::<u8>()))
            .take(1000)
            .collect();

    accounts.shuffle(&mut rng);

    b.iter(|| AccountsDb::sort_and_remove_dups(&mut accounts.clone()));
}
