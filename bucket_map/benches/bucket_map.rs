use {
    bencher::{benchmark_group, benchmark_main, Bencher},
    rayon::prelude::*,
    solana_bucket_map::bucket_map::{BucketMap, BucketMapConfig},
    solana_pubkey::Pubkey,
    std::{collections::hash_map::HashMap, sync::RwLock},
};

type IndexValue = u64;

/// Benchmark insert with Hashmap as baseline for N threads inserting M keys each
fn do_bench_insert_baseline_hashmap(b: &mut Bencher, n: usize, m: usize) {
    let index = RwLock::new(HashMap::new());
    (0..n).into_par_iter().for_each(|i| {
        let key = Pubkey::new_unique();
        index
            .write()
            .unwrap()
            .insert(key, vec![(i, IndexValue::default())]);
    });
    b.iter(|| {
        (0..n).into_par_iter().for_each(|_| {
            for j in 0..m {
                let key = Pubkey::new_unique();
                index
                    .write()
                    .unwrap()
                    .insert(key, vec![(j, IndexValue::default())]);
            }
        })
    });
}

/// Benchmark insert with BucketMap with N buckets for N threads inserting M keys each
fn do_bench_insert_bucket_map(b: &mut Bencher, n: usize, m: usize) {
    let index = BucketMap::new(BucketMapConfig::new(n));
    (0..n).into_par_iter().for_each(|i| {
        let key = Pubkey::new_unique();
        index.update(&key, |_| Some((vec![(i, IndexValue::default())], 0)));
    });
    b.iter(|| {
        (0..n).into_par_iter().for_each(|_| {
            for j in 0..m {
                let key = Pubkey::new_unique();
                index.update(&key, |_| Some((vec![(j, IndexValue::default())], 0)));
            }
        })
    });
}

macro_rules! define_benches {
    ($n:literal, $m:literal) => {
        paste::item! {
            fn [<dim_ $n x $m _baseline>](b: &mut Bencher) {
                do_bench_insert_baseline_hashmap(b, $n, $m);
            }

            fn [<dim_ $n x $m _bucket_map>](b: &mut Bencher) {
                do_bench_insert_bucket_map(b, $n, $m);
            }
        }
    };
}

define_benches!(1, 2);
define_benches!(2, 4);
define_benches!(4, 8);
define_benches!(8, 16);
define_benches!(16, 32);
define_benches!(32, 64);

benchmark_group!(
    benches,
    dim_1x2_baseline,
    dim_1x2_bucket_map,
    dim_2x4_baseline,
    dim_2x4_bucket_map,
    dim_4x8_baseline,
    dim_4x8_bucket_map,
    dim_8x16_baseline,
    dim_8x16_bucket_map,
    dim_16x32_baseline,
    dim_16x32_bucket_map,
    dim_32x64_baseline,
    dim_32x64_bucket_map
);
benchmark_main!(benches);
