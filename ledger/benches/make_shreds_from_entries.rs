#![allow(clippy::arithmetic_side_effects)]
use {
    criterion::{black_box, criterion_group, criterion_main, Criterion},
    rand::Rng,
    solana_entry::entry::Entry,
    solana_ledger::shred::{ProcessShredsStats, ReedSolomonCache, Shred, Shredder},
    solana_sdk::{
        hash::Hash, packet::PACKET_DATA_SIZE, pubkey::Pubkey, signer::keypair::Keypair,
        transaction::Transaction,
    },
    std::iter::repeat_with,
};

fn make_dummy_hash<R: Rng>(rng: &mut R) -> Hash {
    Hash::from(rng.gen::<[u8; 32]>())
}

fn make_dummy_transaction<R: Rng>(rng: &mut R) -> Transaction {
    solana_sdk::system_transaction::transfer(
        &Keypair::new(),                      // from
        &Pubkey::from(rng.gen::<[u8; 32]>()), // to
        rng.gen(),                            // lamports
        make_dummy_hash(rng),                 // recent_blockhash
    )
}

fn make_dummy_entry<R: Rng>(rng: &mut R) -> Entry {
    let count = rng.gen_range(1..20);
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
            serialized_size += bincode::serialized_size(entry).unwrap();
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
    chained_merkle_root: Option<Hash>,
    reed_solomon_cache: &ReedSolomonCache,
    stats: &mut ProcessShredsStats,
) -> (Vec<Shred>, Vec<Shred>) {
    let (data, code) = shredder.entries_to_shreds(
        keypair,
        entries,
        is_last_in_slot,
        chained_merkle_root,
        rng.gen_range(0..2_000), // next_shred_index
        rng.gen_range(0..2_000), // next_code_index
        true,                    // merkle_variant
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
    let mut rng = rand::thread_rng();
    let slot = 315_892_061 + rng.gen_range(0..=100_000);
    let parent_offset = rng.gen_range(1..=u16::MAX);
    let shredder = Shredder::new(
        slot,
        slot - u64::from(parent_offset), // parent_slot
        rng.gen_range(0..64),            // reference_tick
        rng.gen(),                       // shred_version
    )
    .unwrap();
    let keypair = Keypair::new();
    let data_size = num_packets * PACKET_DATA_SIZE;
    let entries = make_dummy_entries(&mut rng, data_size);
    let chained_merkle_root = Some(make_dummy_hash(&mut rng));
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

criterion_group!(benches, bench_make_shreds_from_entries,);
criterion_main!(benches);
