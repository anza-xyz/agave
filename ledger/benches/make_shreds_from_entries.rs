#![allow(clippy::arithmetic_side_effects)]
use {
    criterion::{criterion_group, criterion_main, Criterion},
    rand::Rng,
    solana_entry::entry::Entry,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_ledger::shred::{self, ProcessShredsStats, ReedSolomonCache, Shred, Shredder},
    solana_packet::PACKET_DATA_SIZE,
    solana_pubkey::Pubkey,
    solana_transaction::Transaction,
    std::{hint::black_box, iter::repeat_with},
};

fn make_dummy_hash<R: Rng>(rng: &mut R) -> Hash {
    Hash::from(rng.random::<[u8; 32]>())
}

fn make_dummy_transaction<R: Rng>(rng: &mut R) -> Transaction {
    solana_system_transaction::transfer(
        &Keypair::new(),                         // from
        &Pubkey::from(rng.random::<[u8; 32]>()), // to
        rng.random(),                            // lamports
        make_dummy_hash(rng),                    // recent_blockhash
    )
}

fn make_dummy_entry<R: Rng>(rng: &mut R) -> Entry {
    let count = rng.random_range(1..20);
    let transactions = repeat_with(|| make_dummy_transaction(rng))
        .take(count)
        .collect();
    Entry::new(
        &make_dummy_hash(rng), // prev_hash
        1,                     // num_hashes
        transactions,
    )
}

fn make_dummy_entries<R: Rng>(rng: &mut R, data_size: usize) -> Vec<Entry> {
    let mut serialized_size = 8; // length prefix.
    repeat_with(|| make_dummy_entry(rng))
        .take_while(|entry| {
            serialized_size += wincode::serialized_size(entry).unwrap();
            serialized_size < data_size as u64
        })
        .collect()
}

fn make_shreds_from_entries<R: Rng>(
    rng: &mut R,
    shredder: &Shredder,
    keypair: &Keypair,
    entries: &[Entry],
    is_last_in_slot: bool,
    chained_merkle_root: Hash,
    reed_solomon_cache: &ReedSolomonCache,
    stats: &mut ProcessShredsStats,
) -> (Vec<Shred>, Vec<Shred>) {
    let (data, code) = shredder.entries_to_merkle_shreds_for_tests(
        keypair,
        entries,
        is_last_in_slot,
        chained_merkle_root,
        rng.random_range(0..2_000), // next_shred_index
        rng.random_range(0..2_000), // next_code_index
        reed_solomon_cache,
        stats,
    );
    (black_box(data), black_box(code))
}

fn run_make_shreds_from_entries(
    name: &str,
    c: &mut Criterion,
    num_packets: usize,
    is_last_in_slot: bool,
) {
    let mut rng = rand::rng();
    let slot = 315_892_061 + rng.random_range(0..=100_000);
    let parent_offset = rng.random_range(1..=u16::MAX);
    let shredder = Shredder::new(
        slot,
        slot - u64::from(parent_offset), // parent_slot
        rng.random_range(0..64),         // reference_tick
        rng.random(),                    // shred_version
    )
    .unwrap();
    let keypair = Keypair::new();
    let data_size = num_packets * PACKET_DATA_SIZE;
    let entries = make_dummy_entries(&mut rng, data_size);
    let chained_merkle_root = make_dummy_hash(&mut rng);
    let reed_solomon_cache = ReedSolomonCache::default();
    let mut stats = ProcessShredsStats::default();
    // Initialize the thread-pool and warm the Reed-Solomon cache.
    for _ in 0..10 {
        make_shreds_from_entries(
            &mut rng,
            &shredder,
            &keypair,
            &entries,
            is_last_in_slot,
            chained_merkle_root,
            &reed_solomon_cache,
            &mut stats,
        );
    }
    c.bench_function(name, |b| {
        b.iter(|| {
            let (data, code) = make_shreds_from_entries(
                &mut rng,
                &shredder,
                &keypair,
                &entries,
                is_last_in_slot,
                chained_merkle_root,
                &reed_solomon_cache,
                &mut stats,
            );
            black_box(data);
            black_box(code);
        })
    });
}

fn run_recover_shreds(
    name: &str,
    c: &mut Criterion,
    num_packets: usize,
    num_code: usize,
    is_last_in_slot: bool,
) {
    let mut rng = rand::rng();
    let slot = 315_892_061 + rng.random_range(0..=100_000);
    let parent_offset = rng.random_range(1..=u16::MAX);
    let shredder = Shredder::new(
        slot,
        slot - u64::from(parent_offset), // parent_slot
        rng.random_range(0..64),         // reference_tick
        rng.random(),                    // shred_version
    )
    .unwrap();
    let keypair = Keypair::new();
    let data_size = num_packets * PACKET_DATA_SIZE;
    let entries = make_dummy_entries(&mut rng, data_size);
    let chained_merkle_root = make_dummy_hash(&mut rng);
    let reed_solomon_cache = ReedSolomonCache::default();
    let mut stats = ProcessShredsStats::default();
    let (data, code) = make_shreds_from_entries(
        &mut rng,
        &shredder,
        &keypair,
        &entries,
        is_last_in_slot,
        chained_merkle_root,
        &reed_solomon_cache,
        &mut stats,
    );
    let fec_set_index = data[0].fec_set_index();
    let mut data: Vec<_> = data
        .into_iter()
        .filter(|shred| shred.fec_set_index() == fec_set_index)
        .collect();
    let mut code: Vec<_> = code
        .into_iter()
        .filter(|shred| shred.fec_set_index() == fec_set_index)
        .collect();
    let num_code = num_code.min(code.len());
    for _ in 0..num_code.min(data.len()) {
        let k = rng.random_range(0..data.len());
        data.remove(k);
    }
    for i in 0..num_code {
        let j = rng.random_range(i..code.len());
        code.swap(i, j);
    }
    code.sort_unstable_by_key(|shred| shred.index());
    let mut shreds = data;
    shreds.extend(code);
    c.bench_function(name, |b| {
        b.iter(|| {
            let recovered_shreds = shred::recover(shreds.clone(), &reed_solomon_cache)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            black_box(recovered_shreds);
        })
    });
}

fn bench_make_shreds_from_entries(c: &mut Criterion) {
    for is_last_in_slot in [false, true] {
        for num_packets in [16, 20, 24, 28, 32, 48, 64, 96, 128, 256] {
            let name = format!(
                "bench_make_shreds_from_entries_{}{}",
                if is_last_in_slot { "last_" } else { "" },
                num_packets
            );
            run_make_shreds_from_entries(&name, c, num_packets, is_last_in_slot);
        }
    }
}

fn bench_recover_shreds(c: &mut Criterion) {
    for is_last_in_slot in [false, true] {
        for num_packets in [28, 32, 48, 56] {
            for num_code in [1, 8, 16, 32] {
                let name = format!(
                    "bench_recover_shreds_{}{}_{}",
                    if is_last_in_slot { "last_" } else { "" },
                    num_packets,
                    num_code
                );
                run_recover_shreds(&name, c, num_packets, num_code, is_last_in_slot);
            }
        }
    }
}

criterion_group!(
    benches,
    bench_make_shreds_from_entries,
    bench_recover_shreds
);
criterion_main!(benches);
