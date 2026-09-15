#[cfg(not(feature = "shuttle-test"))]
use {bincode::serialize, solana_hash::HASH_BYTES, solana_sha256_hasher::hash};
use {
    criterion::{BatchSize, Criterion, criterion_group, criterion_main},
    rand::{Rng, SeedableRng, rngs::SmallRng},
    solana_accounts_db::ancestors::Ancestors,
    solana_hash::Hash,
    solana_runtime::bank::BankStatusCache,
    solana_signature::{SIGNATURE_BYTES, Signature},
    std::time::Duration,
};

// Transactions per slot in the fixtures below. bench_status_cache_add_roots
// rebuilds its whole fixture once per iteration, so this sets that cost:
// 300 slots x 1_000 inserts takes about 65ms.
const NUM_TXS_PER_SLOT: usize = 1_000;

#[cfg(not(feature = "shuttle-test"))]
fn bench_status_cache_serialize(c: &mut Criterion) {
    let mut status_cache = BankStatusCache::default();
    status_cache.add_root(0);
    status_cache.clear();
    for hash_index in 0..100 {
        let blockhash = Hash::new_from_array([hash_index; HASH_BYTES]);
        let mut id = blockhash;
        for _ in 0..100 {
            id = hash(id.as_ref());
            let mut sigbytes = Vec::from(id.as_ref());
            id = hash(id.as_ref());
            sigbytes.extend(id.as_ref());
            let sig = Signature::try_from(sigbytes).unwrap();
            status_cache.insert(&blockhash, sig, 0, Ok(()));
        }
    }
    assert!(status_cache.roots().contains(&0));
    c.bench_function("bench_status_cache_serialize", |b| {
        b.iter(|| serialize(&status_cache.root_slot_deltas()).unwrap())
    });
}

#[cfg(not(feature = "shuttle-test"))]
fn bench_status_cache_serialize_max(c: &mut Criterion) {
    // Fill up the status cache to better match what intense runtime usage would
    // look like, then root every filled slot: `root_slot_deltas()` walks
    // `roots()`, so an unrooted slot is never serialized.
    let mut status_cache = BankStatusCache::default();
    let max_root_entries = status_cache.max_root_entries() as u64;
    fill_status_cache(&mut status_cache, max_root_entries, NUM_TXS_PER_SLOT);
    status_cache.add_roots(0..max_root_entries);

    assert_eq!(status_cache.roots().len(), max_root_entries as usize);
    assert_eq!(
        status_cache.root_slot_deltas().len(),
        max_root_entries as usize
    );
    c.bench_function("bench_status_cache_serialize_max", |b| {
        b.iter(|| serialize(&status_cache.root_slot_deltas()).unwrap())
    });
}

fn bench_status_cache_root_slot_deltas(c: &mut Criterion) {
    let mut status_cache = BankStatusCache::default();

    // fill the status cache
    let slots: Vec<_> = (42..).take(status_cache.max_root_entries()).collect();
    for slot in &slots {
        for _ in 0..5 {
            status_cache.insert(&Hash::new_unique(), Hash::new_unique(), *slot, Ok(()));
        }
        status_cache.add_root(*slot);
    }

    c.bench_function("bench_status_cache_root_slot_deltas", |b| {
        b.iter(|| status_cache.root_slot_deltas())
    });
}

fn random_signature(rng: &mut SmallRng) -> Signature {
    let mut sigbytes = [0u8; SIGNATURE_BYTES];
    rng.fill(&mut sigbytes);
    Signature::from(sigbytes)
}

fn fill_status_cache(status_cache: &mut BankStatusCache, max_root_entries: u64, num_txs: usize) {
    for slot in 0..max_root_entries {
        let blockhash = Hash::new_unique();
        fill_status_cache_slot(status_cache, &blockhash, slot, num_txs);
    }
}

fn fill_status_cache_slot(
    status_cache: &mut BankStatusCache,
    blockhash: &Hash,
    slot: u64,
    num_txs: usize,
) {
    for _ in 0..num_txs {
        let tx_hash = Hash::new_unique();
        status_cache.insert(blockhash, tx_hash, slot, Ok(()));
    }
}

// Allowed because the arithmetic is fixed-size bookkeeping over
// `max_root_entries`.
#[allow(clippy::arithmetic_side_effects)]
fn bench_status_cache_check_and_insert(c: &mut Criterion) {
    // Fill up the status cache to better match what intense runtime usage would
    // look like.
    let mut status_cache = BankStatusCache::default();
    let max_root_entries = status_cache.max_root_entries() as u64;
    fill_status_cache(&mut status_cache, max_root_entries - 1, 100_000);

    // Manually fill the last slot so we can save off the blockhash to use for
    // querying and inserting into.
    let blockhash = Hash::new_unique();
    fill_status_cache_slot(&mut status_cache, &blockhash, max_root_entries, 100_000);

    let slot = max_root_entries + 1;
    let ancestors = Ancestors::from((slot - 32..slot).collect::<Vec<u64>>());

    // Generate the batch per iteration, outside the measurement: one signature
    // in ten is already in the cache, the rest are new. In `check_status_cache`
    // a hit means AlreadyProcessed, so duplicates are the minority - but a hit
    // costs several times more than a miss plus insert, so a tenth of them
    // still puts a meaningful share of the measurement on the hit path.
    //
    // The nine new signatures in ten stay in the cache, so it grows by ~101 KiB
    // per iteration and lookups slow down as the run goes on. That makes the
    // criterion settings below part of what this bench reports: it repeats
    // within ~4% run to run at the pinned values, but the median moves from
    // ~0.79ms to ~1.06ms if measurement_time goes to 4s. Keep them fixed to
    // stay comparable with earlier numbers.
    let batch_size = 1_000;
    let duplicate_every = 10;
    let mut rng = SmallRng::seed_from_u64(0);
    let duplicates: Vec<Signature> = (0..batch_size)
        .map(|_| random_signature(&mut rng))
        .collect();
    for sig in &duplicates {
        status_cache.insert(&blockhash, *sig, slot, Ok(()));
    }

    c.bench_function("bench_status_cache_check_and_insert", |b| {
        b.iter_batched(
            || {
                (0..batch_size)
                    .map(|i| {
                        if i % duplicate_every == 0 {
                            duplicates[rng.random_range(0..duplicates.len())]
                        } else {
                            random_signature(&mut rng)
                        }
                    })
                    .collect::<Vec<_>>()
            },
            |batch| {
                for tx_hash in &batch {
                    if status_cache
                        .get_status(*tx_hash, &blockhash, &ancestors)
                        .is_none()
                    {
                        status_cache.insert(&blockhash, *tx_hash, slot, Ok(()));
                    }
                }
            },
            BatchSize::SmallInput,
        )
    });
}

// Allowed because the arithmetic is fixed-size bookkeeping over
// `max_root_entries`.
#[allow(clippy::arithmetic_side_effects)]
fn bench_status_cache_add_roots(c: &mut Criterion) {
    let max_root_entries = BankStatusCache::default().max_root_entries() as u64;
    let start_slot = max_root_entries + 1;

    // Rebuild the fixture per iteration: `add_root` only does real work while
    // the cache is at capacity, and the first call purges the roots and their
    // deltas. Sharing one cache would leave every later iteration re-adding
    // roots that are already present.
    c.bench_function("bench_status_cache_add_roots", |b| {
        b.iter_batched(
            || {
                let mut status_cache = BankStatusCache::default();
                fill_status_cache(&mut status_cache, max_root_entries, NUM_TXS_PER_SLOT);
                status_cache.add_roots(0..max_root_entries);
                status_cache
            },
            |mut status_cache| {
                // The cache is at capacity, so each of these evicts a root
                // and drops its slot delta - the work add_root does in a
                // validator.
                for root in start_slot..start_slot + max_root_entries {
                    status_cache.add_root(root);
                }
            },
            BatchSize::PerIteration,
        )
    });
}

fn bench_status_cache(c: &mut Criterion) {
    #[cfg(not(feature = "shuttle-test"))]
    bench_status_cache_serialize(c);
    #[cfg(not(feature = "shuttle-test"))]
    bench_status_cache_serialize_max(c);
    bench_status_cache_root_slot_deltas(c);
    bench_status_cache_check_and_insert(c);
    bench_status_cache_add_roots(c);
}

criterion_group! {
    name = benches;
    // Cut total run time by trimming criterion's defaults; timings settle early
    // here. Keep the values fixed: check_and_insert's result depends on them.
    config = Criterion::default()
        // 3s default; the medians are flat from 100ms
        .warm_up_time(Duration::from_millis(150))
        // 5s default; the medians are flat from 0.5s
        .measurement_time(Duration::from_millis(750))
        // 100 default, 10 is criterion's minimum. Only matters for add_roots,
        // which rebuilds a 65ms fixture per iteration.
        .sample_size(10)
        .without_plots();
    targets = bench_status_cache
}
criterion_main!(benches);
