use {
    agave_xdp::transmitter::XdpAddrs,
    bencher::{Bencher, benchmark_group, benchmark_main},
    std::{hint::black_box, net::SocketAddr, sync::Arc},
};

fn make_addrs(len: usize) -> Vec<SocketAddr> {
    (0..len)
        .map(|index| {
            SocketAddr::from((
                [10, 0, (index / 256) as u8, index as u8],
                8_000 + index as u16,
            ))
        })
        .collect()
}

// Models the pre-change cache-hit path: borrow Box<[SocketAddr]>, then allocate
// and copy a Vec for the asynchronous XDP transmitter.
fn bench_owned_cache_hit(bencher: &mut Bencher, num_addrs: usize) {
    let cached = make_addrs(num_addrs).into_boxed_slice();
    bencher.iter(|| {
        let xdp_addrs = XdpAddrs::from(black_box(cached.as_ref()).to_vec());
        black_box(xdp_addrs);
    });
}

// Models the shared cache-hit path: retain Arc<[SocketAddr]> in the cache and
// clone only the Arc for the asynchronous XDP transmitter.
fn bench_shared_cache_hit(bencher: &mut Bencher, num_addrs: usize) {
    let cached = Arc::<[SocketAddr]>::from(make_addrs(num_addrs));
    bencher.iter(|| {
        let xdp_addrs = XdpAddrs::from(Arc::clone(black_box(&cached)));
        black_box(xdp_addrs);
    });
}

// Models the pre-change cache-miss ownership work after topology calculation:
// clone the computed Vec for XDP and keep the original allocation for caching.
fn bench_owned_cache_miss(bencher: &mut Bencher, num_addrs: usize) {
    let template = make_addrs(num_addrs);
    bencher.iter(|| {
        let computed = black_box(&template).clone();
        let xdp_addrs = XdpAddrs::from(computed.to_vec());
        let cached = computed.into_boxed_slice();
        black_box((xdp_addrs, cached));
    });
}

// Models the shared cache-miss ownership work after topology calculation:
// convert the computed Vec once, then share that allocation with XDP and cache.
fn bench_shared_cache_miss(bencher: &mut Bencher, num_addrs: usize) {
    let template = make_addrs(num_addrs);
    bencher.iter(|| {
        let computed = black_box(&template).clone();
        let cached = Arc::<[SocketAddr]>::from(computed);
        let xdp_addrs = XdpAddrs::from(Arc::clone(&cached));
        black_box((xdp_addrs, cached));
    });
}

macro_rules! address_benchmarks {
    (
        $owned_hit:ident,
        $shared_hit:ident,
        $owned_miss:ident,
        $shared_miss:ident,
        $num_addrs:expr
    ) => {
        fn $owned_hit(bencher: &mut Bencher) {
            bench_owned_cache_hit(bencher, $num_addrs);
        }

        fn $shared_hit(bencher: &mut Bencher) {
            bench_shared_cache_hit(bencher, $num_addrs);
        }

        fn $owned_miss(bencher: &mut Bencher) {
            bench_owned_cache_miss(bencher, $num_addrs);
        }

        fn $shared_miss(bencher: &mut Bencher) {
            bench_shared_cache_miss(bencher, $num_addrs);
        }
    };
}

// A 15-minute mainnet-validator sample observed 9.093 weighted destinations
// per accepted shred (slot ratios 8.092..=10.342) and a 99.6278% address-cache
// hit rate. Keep 200 as the configured Turbine fanout/worst-case guardrail.
address_benchmarks!(
    bench_owned_cache_hit_8,
    bench_shared_cache_hit_8,
    bench_owned_cache_miss_8,
    bench_shared_cache_miss_8,
    8
);
address_benchmarks!(
    bench_owned_cache_hit_9,
    bench_shared_cache_hit_9,
    bench_owned_cache_miss_9,
    bench_shared_cache_miss_9,
    9
);
address_benchmarks!(
    bench_owned_cache_hit_10,
    bench_shared_cache_hit_10,
    bench_owned_cache_miss_10,
    bench_shared_cache_miss_10,
    10
);
address_benchmarks!(
    bench_owned_cache_hit_200,
    bench_shared_cache_hit_200,
    bench_owned_cache_miss_200,
    bench_shared_cache_miss_200,
    200
);

benchmark_group!(
    benches,
    bench_owned_cache_hit_8,
    bench_shared_cache_hit_8,
    bench_owned_cache_miss_8,
    bench_shared_cache_miss_8,
    bench_owned_cache_hit_9,
    bench_shared_cache_hit_9,
    bench_owned_cache_miss_9,
    bench_shared_cache_miss_9,
    bench_owned_cache_hit_10,
    bench_shared_cache_hit_10,
    bench_owned_cache_miss_10,
    bench_shared_cache_miss_10,
    bench_owned_cache_hit_200,
    bench_shared_cache_hit_200,
    bench_owned_cache_miss_200,
    bench_shared_cache_miss_200,
);
benchmark_main!(benches);
