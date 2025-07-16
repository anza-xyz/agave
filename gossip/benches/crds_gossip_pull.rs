use {
    bencher::{benchmark_group, benchmark_main, Bencher},
    rand::{thread_rng, Rng},
    rayon::ThreadPoolBuilder,
    solana_gossip::{
        crds::{Crds, GossipRoute},
        crds_gossip_pull::{CrdsFilter, CrdsGossipPull},
        crds_value::CrdsValue,
    },
    solana_hash::Hash,
    std::sync::RwLock,
};

fn bench_hash_as_u64(b: &mut Bencher) {
    let hashes: Vec<_> = std::iter::repeat_with(Hash::new_unique)
        .take(1000)
        .collect();
    b.iter(|| {
        hashes
            .iter()
            .map(CrdsFilter::hash_as_u64)
            .collect::<Vec<_>>()
    });
}

fn bench_build_crds_filters(b: &mut Bencher) {
    let thread_pool = ThreadPoolBuilder::new().build().unwrap();
    let mut rng = thread_rng();
    let crds_gossip_pull = CrdsGossipPull::default();
    let mut crds = Crds::default();
    let num_inserts = (0..90_000)
        .filter(|_| {
            crds.insert(
                CrdsValue::new_rand(&mut rng, None),
                rng.gen(),
                GossipRoute::LocalMessage,
            )
            .is_ok()
        })
        .count();
    assert_eq!(num_inserts, 90_000);
    let crds = RwLock::new(crds);
    b.iter(|| {
        let filters = crds_gossip_pull.build_crds_filters(
            &thread_pool,
            &crds,
            992, // max_bloom_filter_bytes
        );
        assert_eq!(filters.len(), 16);
    });
}

benchmark_group!(benches, bench_hash_as_u64, bench_build_crds_filters);
benchmark_main!(benches);
