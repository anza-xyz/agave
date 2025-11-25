use {
    bencher::{benchmark_group, benchmark_main, Bencher},
    std::{
        hint::black_box,
        sync::atomic::{AtomicU64, Ordering},
    },
};

#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

const N: usize = 1_000_000;

// test bench_reset1 ... bench:     436,240 ns/iter (+/- 176,714)
// test bench_reset2 ... bench:     274,007 ns/iter (+/- 129,552)

fn bench_reset1(b: &mut Bencher) {
    agave_logger::setup();

    let mut v = Vec::with_capacity(N);
    v.resize_with(N, AtomicU64::default);

    b.iter(|| {
        black_box({
            for i in &v {
                i.store(0, Ordering::Relaxed);
            }
            0
        });
    });
}

fn bench_reset2(b: &mut Bencher) {
    agave_logger::setup();

    let mut v = Vec::with_capacity(N);
    v.resize_with(N, AtomicU64::default);

    b.iter(|| {
        black_box({
            v.clear();
            v.resize_with(N, AtomicU64::default);
            0
        });
    });
}

benchmark_group!(benches, bench_reset2, bench_reset1);
benchmark_main!(benches);
