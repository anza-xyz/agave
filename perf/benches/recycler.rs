use {
    bencher::{benchmark_group, benchmark_main, Bencher},
    solana_perf::{packet::PacketBatchRecycler, recycler::Recycler},
};

fn bench_recycler(b: &mut Bencher) {
    agave_logger::setup();

    let recycler: PacketBatchRecycler = Recycler::default();

    for _ in 0..1000 {
        let _packet = recycler.allocate("");
    }

    b.iter(move || {
        let _packet = recycler.allocate("");
    });
}

benchmark_group!(benches, bench_recycler);
benchmark_main!(benches);
