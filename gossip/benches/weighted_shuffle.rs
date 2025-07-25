use {
    criterion::{black_box, criterion_group, criterion_main, Criterion},
    rand::{Rng, SeedableRng},
    rand_chacha::ChaChaRng,
    solana_gossip::weighted_shuffle::WeightedShuffle,
    std::iter::repeat_with,
};

fn make_weights<R: Rng>(rng: &mut R) -> Vec<u64> {
    repeat_with(|| rng.gen_range(1..10_000))
        .take(4_000)
        .collect()
}

fn bench_weighted_shuffle_new(c: &mut Criterion) {
    let mut rng = rand::thread_rng();
    c.bench_function("bench_weighted_shuffle_new", |b| {
        b.iter(|| {
            let weights = make_weights(&mut rng);
            black_box(WeightedShuffle::<u64>::new("", &weights));
        })
    });
}

fn bench_weighted_shuffle_shuffle(c: &mut Criterion) {
    let mut seed = [0u8; 32];
    let mut rng = rand::thread_rng();
    let weights = make_weights(&mut rng);
    let weighted_shuffle = WeightedShuffle::new("", weights);
    c.bench_function("bench_weighted_shuffle_shuffle", |b| {
        b.iter(|| {
            rng.fill(&mut seed[..]);
            let mut rng = ChaChaRng::from_seed(seed);
            weighted_shuffle
                .clone()
                .shuffle(&mut rng)
                .for_each(|index| {
                    black_box(index);
                })
        })
    });
    c.bench_function("bench_weighted_shuffle_collect", |b| {
        b.iter(|| {
            rng.fill(&mut seed[..]);
            let mut rng = ChaChaRng::from_seed(seed);
            let mut weighted_shuffle = weighted_shuffle.clone();
            let shuffle = weighted_shuffle.shuffle(&mut rng);
            black_box(shuffle.collect::<Vec<_>>());
        })
    });
}

criterion_group!(
    benches,
    bench_weighted_shuffle_new,
    bench_weighted_shuffle_shuffle,
);
criterion_main!(benches);
