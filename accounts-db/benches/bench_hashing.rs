use {
    criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main},
    solana_account::AccountSharedData,
    solana_accounts_db::accounts_db::AccountsDb,
    solana_pubkey::Pubkey,
};

#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

const KB: usize = 1024;
const MB: usize = KB * KB;

const DATA_SIZES: [usize; 6] = [
    0,       // the smallest account
    165,     // the size of an spl token account
    200,     // the size of a stake account
    KB,      // a medium sized account
    MB,      // a large sized account
    10 * MB, // the largest account
];

/// The number of bytes of *non account data* that are also hashed as
/// part of computing an account's hash.
///
/// Ensure this constant stays in sync with the value of `META_SIZE` in
/// AccountsDb::hash_account_helper().
const META_SIZE: usize = 73;

fn bench_hash_account(c: &mut Criterion) {
    let lamports = 123_456_789;
    let owner = Pubkey::default();
    let address = Pubkey::default();

    let mut group = c.benchmark_group("hash_account");
    for data_size in DATA_SIZES {
        let num_bytes = META_SIZE.checked_add(data_size).unwrap();
        group.throughput(Throughput::Bytes(num_bytes as u64));
        let account = AccountSharedData::new(lamports, data_size, &owner);
        group.bench_function(BenchmarkId::new("lattice", data_size), |b| {
            b.iter(|| AccountsDb::lt_hash_account(&account, &address));
        });
    }
}

criterion_group!(benches, bench_hash_account);
criterion_main!(benches);
