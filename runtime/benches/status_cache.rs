// The arithmetic here is all fixed-size bookkeeping over `max_root_entries`.
#![allow(clippy::arithmetic_side_effects)]

#[cfg(not(feature = "shuttle-test"))]
use {bincode::serialize, solana_hash::HASH_BYTES, solana_sha256_hasher::hash};
use {
    criterion::{Criterion, criterion_group, criterion_main},
    rand::{Rng, SeedableRng, rngs::SmallRng},
    solana_accounts_db::ancestors::Ancestors,
    solana_hash::Hash,
    solana_runtime::bank::BankStatusCache,
    solana_signature::{SIGNATURE_BYTES, Signature},
    std::time::Duration,
};

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
    // `root_slot_deltas()` only walks rooted slots, and slot 0 is the only
    // root on a fresh cache, so that is the only slot worth filling.
    let mut status_cache = BankStatusCache::default();
    fill_status_cache_slot(&mut status_cache, &Hash::new_unique(), 0, 100_000);

    assert!(status_cache.roots().contains(&0));
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

    // Pre-generate unique tx_hashes so we don't spend benchmark time generating
    // them.
    let batch_size = 1_000;
    let mut tx_hashes = Vec::with_capacity(batch_size);
    let mut rng = SmallRng::seed_from_u64(0);
    for _ in 0..batch_size {
        let mut sigbytes = [0u8; SIGNATURE_BYTES];
        rng.fill(&mut sigbytes);
        tx_hashes.push(Signature::from(sigbytes));
    }

    // Only the first pass inserts; later passes find the batch already
    // present, so this measures the steady state of check-then-insert.
    c.bench_function("bench_status_cache_check_and_insert", |b| {
        b.iter(|| {
            for tx_hash in &tx_hashes {
                if status_cache
                    .get_status(*tx_hash, &blockhash, &ancestors)
                    .is_none()
                {
                    status_cache.insert(&blockhash, *tx_hash, slot, Ok(()));
                }
            }
        })
    });
}

fn bench_status_cache_add_roots(c: &mut Criterion) {
    // Only the first iteration purges; afterwards every root in the range is
    // already present and `add_root` is a no-op, which is what the reported
    // median reflects. So only the one rooted slot that the first iteration
    // purges is worth building.
    let mut status_cache = BankStatusCache::default();
    let max_root_entries = status_cache.max_root_entries() as u64;
    fill_status_cache_slot(&mut status_cache, &Hash::new_unique(), 0, 100_000);
    let start_slot = max_root_entries + 1;
    c.bench_function("bench_status_cache_add_roots", |b| {
        b.iter(|| {
            for root in start_slot..start_slot + max_root_entries {
                status_cache.add_root(root);
            }
        })
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
    // Each of these builds a large status cache once and then measures a single
    // cheap-to-repeat operation, so keep the sampling windows short.
    config = Criterion::default()
        .warm_up_time(Duration::from_millis(250))
        .measurement_time(Duration::from_millis(750))
        .sample_size(10)
        .without_plots();
    targets = bench_status_cache
}
criterion_main!(benches);
