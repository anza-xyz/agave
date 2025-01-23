#![feature(test)]
#![allow(clippy::arithmetic_side_effects)]
extern crate test;
use {
    rand::Rng,
    solana_entry::entry::Entry,
    solana_ledger::shred::{ProcessShredsStats, ReedSolomonCache, Shred, Shredder},
    solana_sdk::{
        hash::Hash, packet::PACKET_DATA_SIZE, pubkey::Pubkey, signer::keypair::Keypair,
        transaction::Transaction,
    },
    std::iter::repeat_with,
    test::Bencher,
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
    (std::hint::black_box(data), std::hint::black_box(code))
}

fn run_make_shreds_from_entries(bencher: &mut Bencher, data_size: usize, is_last_in_slot: bool) {
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
    bencher.iter(|| {
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
        std::hint::black_box(data);
        std::hint::black_box(code);
    });
}

macro_rules! make_bench {
    ($name:ident, $size:literal, $last:literal) => {
        #[bench]
        fn $name(bencher: &mut Bencher) {
            run_make_shreds_from_entries(
                bencher,
                $size * PACKET_DATA_SIZE, // data_size
                $last,                    // is_last_in_slot
            );
        }
    };
}

make_bench!(bench_make_shreds_from_entries_016, 16, false);
make_bench!(bench_make_shreds_from_entries_024, 24, false);
make_bench!(bench_make_shreds_from_entries_032, 32, false);
make_bench!(bench_make_shreds_from_entries_048, 48, false);
make_bench!(bench_make_shreds_from_entries_064, 64, false);
make_bench!(bench_make_shreds_from_entries_096, 96, false);
make_bench!(bench_make_shreds_from_entries_128, 128, false);
make_bench!(bench_make_shreds_from_entries_256, 256, false);

make_bench!(bench_make_shreds_from_entries_last_016, 16, true);
make_bench!(bench_make_shreds_from_entries_last_024, 24, true);
make_bench!(bench_make_shreds_from_entries_last_032, 32, true);
make_bench!(bench_make_shreds_from_entries_last_048, 48, true);
make_bench!(bench_make_shreds_from_entries_last_064, 64, true);
make_bench!(bench_make_shreds_from_entries_last_096, 96, true);
make_bench!(bench_make_shreds_from_entries_last_128, 128, true);
make_bench!(bench_make_shreds_from_entries_last_256, 256, true);
