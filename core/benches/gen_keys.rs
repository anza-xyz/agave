use {
    bencher::{benchmark_group, benchmark_main, Bencher},
    solana_core::gen_keys::GenKeys,
};

fn bench_gen_keys(b: &mut Bencher) {
    let mut rnd = GenKeys::new([0u8; 32]);
    b.iter(|| rnd.gen_n_keypairs(1000));
}

benchmark_group!(benches, bench_gen_keys);
benchmark_main!(benches);
