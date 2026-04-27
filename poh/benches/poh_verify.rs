use {
    bencher::{Bencher, benchmark_group, benchmark_main},
    solana_entry::entry::{self, Entry, EntrySlice, next_entry_mut},
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_sha256_hasher::hash,
    solana_signer::Signer,
    solana_system_transaction::transfer,
};

#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

const NUM_HASHES: u64 = 400;
const NUM_ENTRIES: usize = 800;

fn bench_poh_verify_ticks(b: &mut Bencher) {
    agave_logger::setup();
    let thread_pool = entry::thread_pool_for_benches();

    let zero = Hash::default();
    let start_hash = hash(zero.as_ref());
    let mut cur_hash = start_hash;

    let mut ticks: Vec<Entry> = Vec::with_capacity(NUM_ENTRIES);
    for _ in 0..NUM_ENTRIES {
        ticks.push(next_entry_mut(&mut cur_hash, NUM_HASHES, vec![]));
    }

    b.iter(|| {
        assert!(ticks.verify(&start_hash, &thread_pool).status());
    })
}

fn bench_poh_verify_transaction_entries(b: &mut Bencher) {
    let thread_pool = entry::thread_pool_for_benches();

    let zero = Hash::default();
    let start_hash = hash(zero.as_ref());
    let mut cur_hash = start_hash;

    let keypair1 = Keypair::new();
    let pubkey1 = keypair1.pubkey();

    let mut ticks: Vec<Entry> = Vec::with_capacity(NUM_ENTRIES);
    for _ in 0..NUM_ENTRIES {
        let tx = transfer(&keypair1, &pubkey1, 42, cur_hash);
        ticks.push(next_entry_mut(&mut cur_hash, NUM_HASHES, vec![tx]));
    }

    b.iter(|| {
        assert!(ticks.verify(&start_hash, &thread_pool).status());
    })
}

benchmark_group!(
    benches,
    bench_poh_verify_ticks,
    bench_poh_verify_transaction_entries
);
benchmark_main!(benches);
