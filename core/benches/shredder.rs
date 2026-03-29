#![allow(clippy::arithmetic_side_effects)]

use {
    bencher::{Bencher, benchmark_group, benchmark_main},
    rand::Rng,
    solana_entry::entry::{Entry, create_ticks},
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_ledger::shred::{
        CODING_SHREDS_PER_FEC_BLOCK, DATA_SHREDS_PER_FEC_BLOCK, ProcessShredsStats,
        ReedSolomonCache, Shred, Shredder, get_data_shred_bytes_per_batch_typical,
        max_entries_per_n_shred, max_ticks_per_n_shreds, recover,
    },
    solana_perf::test_tx,
    std::hint::black_box,
};

fn make_test_entry(txs_per_entry: u64) -> Entry {
    Entry {
        num_hashes: 100_000,
        hash: Hash::default(),
        transactions: vec![test_tx::test_tx().into(); txs_per_entry as usize],
    }
}
fn make_large_unchained_entries(txs_per_entry: u64, num_entries: u64) -> Vec<Entry> {
    (0..num_entries)
        .map(|_| make_test_entry(txs_per_entry))
        .collect()
}
const SHRED_SIZE_TYPICAL: usize = {
    let batch_payload = get_data_shred_bytes_per_batch_typical() as usize;
    batch_payload / DATA_SHREDS_PER_FEC_BLOCK
};

fn bench_shredder_ticks(bencher: &mut Bencher) {
    let kp = Keypair::new();

    let num_shreds = 1_000_000_usize.div_ceil(SHRED_SIZE_TYPICAL);
    // ~1Mb
    let num_ticks = max_ticks_per_n_shreds(1, Some(SHRED_SIZE_TYPICAL)) * num_shreds as u64;
    let entries = create_ticks(num_ticks, 0, Hash::default());
    let reed_solomon_cache = ReedSolomonCache::default();
    let chained_merkle_root = Hash::new_from_array(rand::rng().random());
    bencher.iter(|| {
        let shredder = Shredder::new(1, 0, 0, 0).unwrap();
        shredder.entries_to_merkle_shreds_for_tests(
            &kp,
            &entries,
            true,
            chained_merkle_root,
            0,
            0,
            &reed_solomon_cache,
            &mut ProcessShredsStats::default(),
        );
    })
}

fn bench_shredder_large_entries(bencher: &mut Bencher) {
    let kp = Keypair::new();
    let shred_size = SHRED_SIZE_TYPICAL;
    let num_shreds = 1_000_000_usize.div_ceil(shred_size);
    let txs_per_entry = 128;
    let num_entries = max_entries_per_n_shred(
        &make_test_entry(txs_per_entry),
        num_shreds as u64,
        Some(shred_size),
    );
    let entries = make_large_unchained_entries(txs_per_entry, num_entries);
    let chained_merkle_root = Hash::new_from_array(rand::rng().random());
    let reed_solomon_cache = ReedSolomonCache::default();
    // 1Mb
    bencher.iter(|| {
        let shredder = Shredder::new(1, 0, 0, 0).unwrap();
        shredder.entries_to_merkle_shreds_for_tests(
            &kp,
            &entries,
            true,
            chained_merkle_root,
            0,
            0,
            &reed_solomon_cache,
            &mut ProcessShredsStats::default(),
        );
    })
}

fn bench_deshredder(bencher: &mut Bencher) {
    let kp = Keypair::new();
    let shred_size = SHRED_SIZE_TYPICAL;
    // ~10Mb
    let num_shreds = 10_000_000_usize.div_ceil(shred_size);
    let num_ticks = max_ticks_per_n_shreds(1, Some(shred_size)) * num_shreds as u64;
    let entries = create_ticks(num_ticks, 0, Hash::default());
    let shredder = Shredder::new(1, 0, 0, 0).unwrap();
    let chained_merkle_root = Hash::new_from_array(rand::rng().random());
    let (data_shreds, _) = shredder.entries_to_merkle_shreds_for_tests(
        &kp,
        &entries,
        true,
        chained_merkle_root,
        0,
        0,
        &ReedSolomonCache::default(),
        &mut ProcessShredsStats::default(),
    );
    bencher.iter(|| {
        let data_shreds = data_shreds.iter().map(Shred::payload);
        let raw = &mut Shredder::deshred(data_shreds).unwrap();
        assert_ne!(raw.len(), 0);
    })
}

fn bench_deserialize_hdr(bencher: &mut Bencher) {
    let keypair = Keypair::new();
    let shredder = Shredder::new(2, 1, 0, 0).unwrap();
    let merkle_root = Hash::new_from_array(rand::rng().random());
    let mut stats = ProcessShredsStats::default();
    let reed_solomon_cache = ReedSolomonCache::default();
    let mut shreds = shredder
        .make_merkle_shreds_from_entries(
            &keypair,
            &[],
            true, // is_last_in_slot
            merkle_root,
            1, // next_shred_index
            0, // next_code_index
            &reed_solomon_cache,
            &mut stats,
        )
        .filter(Shred::is_data)
        .collect::<Vec<_>>();
    let shred = shreds.remove(0);

    bencher.iter(|| {
        let payload = shred.payload().clone();
        let _ = Shred::new_from_serialized_shred(payload).unwrap();
    })
}

fn make_entries() -> Vec<Entry> {
    let txs_per_entry = 128;
    let num_entries = max_entries_per_n_shred(&make_test_entry(txs_per_entry), 200, Some(1000));
    make_large_unchained_entries(txs_per_entry, num_entries)
}

fn bench_shredder_coding(bencher: &mut Bencher) {
    let entries = make_entries();
    let shredder = Shredder::new(1, 0, 0, 0).unwrap();
    let reed_solomon_cache = ReedSolomonCache::default();
    let merkle_root = Hash::new_from_array(rand::rng().random());
    bencher.iter(|| {
        let result: Vec<_> = shredder
            .make_merkle_shreds_from_entries(
                &Keypair::new(),
                &entries,
                true, // is_last_in_slot
                merkle_root,
                0, // next_shred_index
                0, // next_code_index
                &reed_solomon_cache,
                &mut ProcessShredsStats::default(),
            )
            .collect();
        black_box(result);
    })
}

fn bench_shredder_decoding(bencher: &mut Bencher) {
    let entries = make_entries();
    let shredder = Shredder::new(1, 0, 0, 0).unwrap();
    let reed_solomon_cache = ReedSolomonCache::default();
    let merkle_root = Hash::new_from_array(rand::rng().random());
    let (_data_shreds, mut coding_shreds): (Vec<_>, Vec<_>) = shredder
        .make_merkle_shreds_from_entries(
            &Keypair::new(),
            &entries,
            true, // is_last_in_slot
            merkle_root,
            0, // next_shred_index
            0, // next_code_index
            &reed_solomon_cache,
            &mut ProcessShredsStats::default(),
        )
        .partition(Shred::is_data);
    coding_shreds.truncate(CODING_SHREDS_PER_FEC_BLOCK);

    bencher.iter(|| {
        for shred in recover(coding_shreds.clone(), &reed_solomon_cache).unwrap() {
            black_box(shred.unwrap());
        }
    })
}

benchmark_group!(
    benches,
    bench_shredder_ticks,
    bench_shredder_large_entries,
    bench_deshredder,
    bench_deserialize_hdr,
    bench_shredder_coding,
    bench_shredder_decoding
);
benchmark_main!(benches);
