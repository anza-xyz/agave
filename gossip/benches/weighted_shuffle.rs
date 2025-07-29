use {
    bencher::{benchmark_group, benchmark_main, black_box, Bencher},
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

fn bench_weighted_shuffle_new(b: &mut Bencher) {
    let mut rng = rand::thread_rng();
    b.iter(|| {
        let weights = make_weights(&mut rng);
        black_box(WeightedShuffle::<u64>::new("", &weights));
    })
}

fn bench_weighted_shuffle_shuffle(b: &mut Bencher) {
    let mut seed = [0u8; 32];
    let mut rng = rand::thread_rng();
    let weights = make_weights(&mut rng);
    let weighted_shuffle = WeightedShuffle::new("", weights);
    b.iter(|| {
        rng.fill(&mut seed[..]);
        let mut rng = ChaChaRng::from_seed(seed);
        weighted_shuffle
            .clone()
            .shuffle(&mut rng)
            .for_each(|index| {
                black_box(index);
            })
    });
    b.iter(|| {
        rng.fill(&mut seed[..]);
        let mut rng = ChaChaRng::from_seed(seed);
        let mut weighted_shuffle = weighted_shuffle.clone();
        let shuffle = weighted_shuffle.shuffle(&mut rng);
        black_box(shuffle.collect::<Vec<_>>());
    })
}

benchmark_group!(
    benches,
    bench_weighted_shuffle_new,
    bench_weighted_shuffle_shuffle,
);
benchmark_main!(benches);
