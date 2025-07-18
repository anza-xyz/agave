use {
    bencher::{benchmark_group, benchmark_main, Bencher},
    rand::{thread_rng, Rng},
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

fn bench_crds_shards_find(b: &mut Bencher, num_values: usize, mask_bits: u32) {
    let mut rng = thread_rng();
    let values: Vec<_> = repeat_with(|| new_test_crds_value(&mut rng))
        .take(num_values)
        .collect();
    let mut shards = CrdsShards::new(CRDS_SHARDS_BITS);
    for (index, value) in values.iter().enumerate() {
        assert!(shards.insert(index, value));
    }
    b.iter(|| {
        let mask = rng.gen();
        let _hits = shards.find(mask, mask_bits).count();
    });
}

fn bench_crds_shards_find_0(bencher: &mut Bencher) {
    bench_crds_shards_find(bencher, 100_000, 0);
}

fn bench_crds_shards_find_1(bencher: &mut Bencher) {
    bench_crds_shards_find(bencher, 100_000, 1);
}

fn bench_crds_shards_find_3(bencher: &mut Bencher) {
    bench_crds_shards_find(bencher, 100_000, 3);
}

fn bench_crds_shards_find_5(bencher: &mut Bencher) {
    bench_crds_shards_find(bencher, 100_000, 5);
}

fn bench_crds_shards_find_7(bencher: &mut Bencher) {
    bench_crds_shards_find(bencher, 100_000, 7);
}

fn bench_crds_shards_find_8(bencher: &mut Bencher) {
    bench_crds_shards_find(bencher, 100_000, 8);
}

fn bench_crds_shards_find_9(bencher: &mut Bencher) {
    bench_crds_shards_find(bencher, 100_000, 9);
}

benchmark_group!(
    benches,
    bench_crds_shards_find_0,
    bench_crds_shards_find_1,
    bench_crds_shards_find_3,
    bench_crds_shards_find_5,
    bench_crds_shards_find_7,
    bench_crds_shards_find_8,
    bench_crds_shards_find_9
);
benchmark_main!(benches);
