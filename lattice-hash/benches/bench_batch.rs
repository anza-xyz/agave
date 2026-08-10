//! Compares the current one-at-a-time lattice-hash path against the batched
//! BLAKE3 kernel, on a realistic stream of small (token/stake-sized) accounts.

use {
    criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main},
    rand::prelude::*,
    rand_chacha::ChaChaRng,
    solana_lattice_hash::{batch, lt_hash::LtHash},
};

const NUM_ACCOUNTS: usize = 1024;
const MSG_LEN: usize = 273; // 8 lamports + ~200B data + 1 + 32 owner + 32 pubkey

fn make_messages(rng: &mut impl Rng) -> Vec<Vec<u8>> {
    (0..NUM_ACCOUNTS)
        .map(|_| {
            let mut m = vec![0u8; MSG_LEN];
            rng.fill_bytes(&mut m);
            m
        })
        .collect()
}

/// Baseline: today's path — `LtHash::with` (a `blake3` XOF) then `mix_in`, one
/// account at a time.
fn baseline(messages: &[&[u8]], acc: &mut LtHash) {
    for msg in messages {
        let mut h = blake3::Hasher::new();
        h.update(msg);
        acc.mix_in(&LtHash::with(&h));
    }
}

fn bench(c: &mut Criterion) {
    let mut rng = ChaChaRng::seed_from_u64(7);
    let msgs = make_messages(&mut rng);
    let refs: Vec<&[u8]> = msgs.iter().map(|m| m.as_slice()).collect();

    let mut group = c.benchmark_group("accounts_lt_hash");
    group.throughput(Throughput::Elements(NUM_ACCOUNTS as u64));

    group.bench_function("baseline_one_at_a_time", |b| {
        b.iter_batched_ref(
            LtHash::identity,
            |acc| baseline(&refs, acc),
            BatchSize::SmallInput,
        )
    });

    // Streaming `Accumulator`: the shape the accounts-hash hot loop uses —
    // create once, `add_message` (copies the message into the owned buffer the
    // kernel then reads in place) each account, `into_lt_hash`.
    group.bench_function("batched_accumulator", |b| {
        b.iter(|| {
            let mut acc = batch::Accumulator::new();
            for &msg in &refs {
                acc.add_message(msg);
            }
            acc.into_lt_hash()
        })
    });

    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
