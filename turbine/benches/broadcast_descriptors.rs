use {
    bencher::{Bencher, benchmark_group, benchmark_main},
    std::{hint::black_box, net::SocketAddr},
};

const SHREDS_PER_FEC_SET: usize = 64;
const DESTINATIONS_PER_SHRED: usize = 4;

fn destinations() -> [Option<SocketAddr>; DESTINATIONS_PER_SHRED] {
    std::array::from_fn(|index| Some(SocketAddr::from(([10, 0, 0, index as u8], 8_000))))
}

fn descriptors(
    destinations: &[Option<SocketAddr>; DESTINATIONS_PER_SHRED],
) -> impl Iterator<Item = (usize, SocketAddr)> + '_ {
    (0..SHREDS_PER_FEC_SET).flat_map(|shred_index| {
        destinations
            .iter()
            .copied()
            // Match broadcast peer selection, where absent destinations are
            // filtered and therefore the iterator has a zero lower size hint.
            .filter_map(move |addr| addr.map(|addr| (shred_index, addr)))
    })
}

fn bench_legacy(bencher: &mut Bencher, fec_sets: usize) {
    let destinations = destinations();
    bencher.iter(|| {
        for _ in 0..fec_sets {
            let packets: Vec<_> = descriptors(black_box(&destinations)).collect();
            black_box(packets);
        }
    });
}

fn bench_reused(bencher: &mut Bencher, fec_sets: usize) {
    let destinations = destinations();
    let mut packets = Vec::new();
    packets.extend(descriptors(&destinations)); // Warm the reusable allocation.
    bencher.iter(|| {
        for _ in 0..fec_sets {
            packets.clear();
            packets.extend(descriptors(black_box(&destinations)));
            black_box(&packets);
        }
    });
}

macro_rules! descriptor_benchmarks {
    ($legacy:ident, $reused:ident, $fec_sets:expr) => {
        fn $legacy(bencher: &mut Bencher) {
            bench_legacy(bencher, $fec_sets);
        }

        fn $reused(bencher: &mut Bencher) {
            bench_reused(bencher, $fec_sets);
        }
    };
}

descriptor_benchmarks!(bench_legacy_1_fec_set, bench_reused_1_fec_set, 1);
descriptor_benchmarks!(bench_legacy_23_fec_sets, bench_reused_23_fec_sets, 23);
descriptor_benchmarks!(bench_legacy_34_fec_sets, bench_reused_34_fec_sets, 34);

benchmark_group!(
    benches,
    bench_legacy_1_fec_set,
    bench_reused_1_fec_set,
    bench_legacy_23_fec_sets,
    bench_reused_23_fec_sets,
    bench_legacy_34_fec_sets,
    bench_reused_34_fec_sets,
);
benchmark_main!(benches);
