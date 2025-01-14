/// Benchmarks for the `ChiliPepperStore` implementation.
/// pass benchmark_group name in the command line to run a specific benchmark group.
/// > ../cargo nightly bench -- chili_pepper_store
use {
    criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput},
    solana_accounts_db::chili_pepper::chili_pepper_store::{ChiliPepperStoreInner, PubkeySlot},
    solana_sdk::pubkey::Pubkey,
    std::env::current_dir,
    tempfile::NamedTempFile,
};

const ELEMENTS: [u64; 6] = [1_000, 10_000, 100_000, 200_000, 500_000, 1_000_000]; // 1K, 10K, 100K, 1M

fn bench_chili_pepper_store(c: &mut Criterion) {
    let tmpfile: NamedTempFile = NamedTempFile::new_in(current_dir().unwrap()).unwrap();
    let store = ChiliPepperStoreInner::new_with_path(tmpfile.path()).unwrap();

    let mut group = c.benchmark_group("chili_pepper_store");
    group.significance_level(0.1).sample_size(10);

    for size in ELEMENTS {
        group.throughput(Throughput::Elements(size as u64));

        let mut pubkeys = vec![];
        for _ in 0..size {
            pubkeys.push(Pubkey::new_unique());
        }

        let data = pubkeys
            .iter()
            .map(|k| (PubkeySlot::new(k, 42), 163))
            .collect::<Vec<_>>();

        group.bench_function(BenchmarkId::new("insert", size), |b| {
            b.iter(|| {
                store.bulk_insert(data.iter().copied()).unwrap();
            });
        });

        group.bench_function(BenchmarkId::new("get", size), |b| {
            b.iter(|| {
                for (key, value) in data.iter() {
                    let result = store.get(*key).unwrap();
                    assert_eq!(result, Some(*value));
                }
            });
        });

        group.bench_function(BenchmarkId::new("bulk_get", size), |b| {
            b.iter(|| {
                let v = store.bulk_get_for_pubkeys(&pubkeys).unwrap();
                assert_eq!(v.len(), size as usize);
            });
        });
    }
}

criterion_group!(benches, bench_chili_pepper_store);
criterion_main!(benches);
