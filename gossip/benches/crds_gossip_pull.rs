use {
    criterion::{Criterion, criterion_group, criterion_main},
    rand::{Rng, rng},
    rayon::ThreadPoolBuilder,
    solana_gossip::{
        crds::{Crds, GossipRoute},
        crds_gossip_pull::{CrdsFilter, CrdsGossipPull},
        crds_value::CrdsValue,
    },
    solana_hash::Hash,
    std::sync::RwLock,
};

fn bench_hash_as_u64(c: &mut Criterion) {
    let hashes: Vec<_> = std::iter::repeat_with(Hash::new_unique)
        .take(1000)
        .collect();
    c.bench_function("bench_hash_as_u64", |b| {
        b.iter(|| {
            hashes
                .iter()
                .map(CrdsFilter::hash_as_u64)
                .collect::<Vec<_>>()
        })
    });
}

fn bench_build_crds_filters(c: &mut Criterion) {
    let thread_pool = ThreadPoolBuilder::new().build().unwrap();
    let mut rng = rng();
    let crds_gossip_pull = CrdsGossipPull::default();
    let mut crds = Crds::default();
    let num_inserts = (0..90_000)
        .filter(|_| {
            crds.insert(
                CrdsValue::new_rand(&mut rng, None),
                rng.random(),
                GossipRoute::LocalMessage,
            )
            .is_ok()
        })
        .count();
    assert_eq!(num_inserts, 90_000);
    let crds = RwLock::new(crds);
    c.bench_function("bench_build_crds_filters", |b| {
        b.iter(|| {
            let filters = crds_gossip_pull.build_crds_filters(
                &thread_pool,
                &crds,
                992, // max_bloom_filter_bytes
            );
            assert_eq!(filters.len(), 16);
        })
    });
}

criterion_group!(benches, bench_hash_as_u64, bench_build_crds_filters);
criterion_main!(benches);
