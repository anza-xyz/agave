#![feature(test)]
extern crate test;

use {
    solana_entry::entry::{self, next_entry_mut, Entry, EntrySlice},
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_sha256_hasher::hash,
    solana_signer::Signer,
    solana_system_transaction::transfer,
    test::Bencher,
};

#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

const NUM_HASHES: u64 = 400;
const NUM_ENTRIES: usize = 800;

#[bench]
fn bench_poh_verify_ticks(bencher: &mut Bencher) {
    agave_logger::setup();
    let thread_pool = entry::thread_pool_for_benches();

    let zero = Hash::default();
    let start_hash = hash(zero.as_ref());
    let mut cur_hash = start_hash;

    let mut ticks: Vec<Entry> = Vec::with_capacity(NUM_ENTRIES);
    for _ in 0..NUM_ENTRIES {
        ticks.push(next_entry_mut(&mut cur_hash, NUM_HASHES, vec![]));
    }

    bencher.iter(|| {
        assert!(ticks.verify(&start_hash, &thread_pool).status());
    })
}

#[bench]
fn bench_poh_verify_transaction_entries(bencher: &mut Bencher) {
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

    bencher.iter(|| {
        assert!(ticks.verify(&start_hash, &thread_pool).status());
    })
}
