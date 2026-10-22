use {
    criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main},
    solana_account::{Account, WINCODE_CONFIG},
    solana_pubkey::Pubkey,
};

#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

const KB: usize = 1024;
const MB: usize = KB * KB;

const DATA_SIZES: [usize; 4] = [
    0,       // the smallest account
    200,     // the size of a stake account
    MB,      // a mid-size account
    10 * MB, // the largest account
];

/// Benchmark how long it takes to serialize an account
fn bench_account_serialize(c: &mut Criterion) {
    let mut group = c.benchmark_group("serde_account_serialize");
    for data_size in DATA_SIZES {
        let account = Account::new(0, data_size, &Pubkey::default());
        let num_bytes = wincode::config::serialized_size(&account, WINCODE_CONFIG).unwrap();
        group.throughput(Throughput::Bytes(num_bytes));
        group.bench_function(BenchmarkId::new("wincode", data_size), |b| {
            b.iter_batched(
                || Vec::with_capacity(num_bytes as usize),
                |mut buffer| {
                    wincode::config::serialize_into(&mut buffer, &account, WINCODE_CONFIG).unwrap()
                },
                BatchSize::PerIteration,
            );
        });
    }
}

/// Benchmark how long it takes to deserialize an account
fn bench_account_deserialize(c: &mut Criterion) {
    let mut group = c.benchmark_group("serde_account_deserialize");
    for data_size in DATA_SIZES {
        let account = Account::new(0, data_size, &Pubkey::default());
        let serialized_account = wincode::config::serialize(&account, WINCODE_CONFIG).unwrap();
        group.throughput(Throughput::Bytes(serialized_account.len() as u64));
        group.bench_function(BenchmarkId::new("wincode", data_size), |b| {
            b.iter_batched(
                || (),
                |()| {
                    wincode::config::deserialize::<Account, _>(&serialized_account, WINCODE_CONFIG)
                        .unwrap()
                },
                BatchSize::PerIteration,
            );
        });
    }
}

criterion_group!(benches, bench_account_serialize, bench_account_deserialize);
criterion_main!(benches);
