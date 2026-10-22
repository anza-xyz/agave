use {
    criterion::{Criterion, criterion_group, criterion_main},
    rand::{Rng, rng},
    solana_gossip::{
        crds::{Crds, GossipRoute, VersionedCrdsValue},
        crds_shards::CrdsShards,
        crds_value::CrdsValue,
    },
    solana_time_utils::timestamp,
    std::iter::repeat_with,
};

const CRDS_SHARDS_BITS: u32 = 8;

fn new_test_crds_value<R: Rng>(rng: &mut R) -> VersionedCrdsValue {
    let value = CrdsValue::new_rand(rng, None);
    let label = value.label();
    let mut crds = Crds::default();
    crds.insert(value, timestamp(), GossipRoute::LocalMessage)
        .unwrap();
    crds.get::<&VersionedCrdsValue>(&label).cloned().unwrap()
}

fn bench_crds_shards_find(c: &mut Criterion, num_values: usize, mask_bits: u32) {
    let mut rng = rng();
    let values: Vec<_> = repeat_with(|| new_test_crds_value(&mut rng))
        .take(num_values)
        .collect();
    let mut shards = CrdsShards::new(CRDS_SHARDS_BITS);
    for (index, value) in values.iter().enumerate() {
        assert!(shards.insert(index, value));
    }
    c.bench_function(
        &format!("bench_crds_shards_find: mask_bits: {mask_bits:?}"),
        |b| {
            b.iter(|| {
                let mask = rng.random();
                let _hits = shards.find(mask, mask_bits).count();
            })
        },
    );
}

fn bench_crds_shards_find_0(c: &mut Criterion) {
    bench_crds_shards_find(c, 100_000, 0);
}

fn bench_crds_shards_find_1(c: &mut Criterion) {
    bench_crds_shards_find(c, 100_000, 1);
}

fn bench_crds_shards_find_3(c: &mut Criterion) {
    bench_crds_shards_find(c, 100_000, 3);
}

fn bench_crds_shards_find_5(c: &mut Criterion) {
    bench_crds_shards_find(c, 100_000, 5);
}

fn bench_crds_shards_find_7(c: &mut Criterion) {
    bench_crds_shards_find(c, 100_000, 7);
}

fn bench_crds_shards_find_8(c: &mut Criterion) {
    bench_crds_shards_find(c, 100_000, 8);
}

fn bench_crds_shards_find_9(c: &mut Criterion) {
    bench_crds_shards_find(c, 100_000, 9);
}

criterion_group!(
    benches,
    bench_crds_shards_find_0,
    bench_crds_shards_find_1,
    bench_crds_shards_find_3,
    bench_crds_shards_find_5,
    bench_crds_shards_find_7,
    bench_crds_shards_find_8,
    bench_crds_shards_find_9
);
criterion_main!(benches);
