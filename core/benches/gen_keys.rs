use {
    criterion::{criterion_group, criterion_main, Criterion},
    solana_core::gen_keys::GenKeys,
};


fn bench_gen_keys(c: &mut Criterion) {
    let mut rnd = GenKeys::new([0u8; 32]);
    c.bench_function("gen_keys", |b| b.iter(|| rnd.gen_n_keypairs(1000)));
}

criterion_group!(benches, bench_gen_keys);
criterion_main!(benches);
