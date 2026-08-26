//! What building a whole erasure batch costs, next to the shredder already in the tree.
//!
//! One batch is 64 shreds: 32 bodies copied in, a 32:32 Reed-Solomon encode, 64 leaf hashes, one
//! signature, 64 proofs written, and 64 payloads read back through the parser. The incumbent group
//! runs `solana-ledger`'s shredder over the same bytes, which is the number that matters. The
//! two produce identical payloads, so this is a straight comparison.

use {
    criterion::{Criterion, Throughput, criterion_group, criterion_main},
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_ledger::shred::{ProcessShredsStats, ReedSolomonCache, Shredder},
    solana_shred::shredder::{BatchPosition, FecSet, FecSetSpec},
    std::hint::black_box,
};

const SLOT: u64 = 1_000;
const PARENT_SLOT: u64 = 998;
const VERSION: u16 = 42;
const REFERENCE_TICK: u8 = 9;
const FEC_SET_INDEX: u32 = 96;
/// Shreds in one erasure batch.
const SHREDS: u64 = 64;

fn bench_build(c: &mut Criterion) {
    let keypair = Keypair::new_from_array([7u8; 32]);
    // A full batch of data, so no shred is left empty.
    let spec = FecSetSpec {
        slot: SLOT,
        parent_slot: PARENT_SLOT,
        version: VERSION,
        reference_tick: REFERENCE_TICK,
        fec_set_index: FEC_SET_INDEX,
        chained_merkle_root: Hash::new_from_array([5u8; 32]),
        batch_position: BatchPosition::DataComplete,
    };
    let data: Vec<u8> = (0..spec.capacity()).map(|index| index as u8).collect();

    let mut group = c.benchmark_group("shred_build");
    group.throughput(Throughput::Elements(SHREDS));
    group.bench_function("batch", |b| {
        b.iter(|| {
            black_box(FecSet::build(&spec, &data, &keypair)).expect("the batch is well specified")
        })
    });

    let shredder = Shredder::new(SLOT, PARENT_SLOT, REFERENCE_TICK, VERSION)
        .expect("the slot chains to its parent");
    let cache = ReedSolomonCache::default();
    group.bench_function("batch/incumbent", |b| {
        b.iter(|| {
            let shreds = shredder
                .make_shreds_from_data_slice(
                    &keypair,
                    &data,
                    false,
                    spec.chained_merkle_root,
                    FEC_SET_INDEX,
                    FEC_SET_INDEX,
                    &cache,
                    &mut ProcessShredsStats::default(),
                )
                .expect("the batch is well specified");
            black_box(shreds.count());
        })
    });
    group.finish();
}

criterion_group!(benches, bench_build);
criterion_main!(benches);
