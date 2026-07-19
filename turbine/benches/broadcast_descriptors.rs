use {
    bencher::{Bencher, benchmark_group, benchmark_main},
    solana_ledger::shred::Payload,
    std::{hint::black_box, net::SocketAddr},
};

const SHREDS_PER_FEC_SET: usize = 64;

fn make_payloads(fec_sets: usize) -> Vec<Payload> {
    let num_shreds = fec_sets
        .checked_mul(SHREDS_PER_FEC_SET)
        .expect("benchmark FEC shape fits usize");
    (0..num_shreds).map(|_| Payload::from(Vec::new())).collect()
}

fn destinations(count: usize) -> [Option<SocketAddr>; 2] {
    assert!(count <= 2);
    std::array::from_fn(|index| {
        (index < count).then(|| SocketAddr::from(([10, 0, 0, index as u8], 8_000)))
    })
}

// Mirrors one production broadcast_shreds call: all data and coding shreds in
// the dispatched batch feed one descriptor Vec. Each shred has at most the
// next leader and the standard broadcast peer as destinations.
fn descriptors<'a>(
    payloads: &'a [Payload],
    destinations: &'a [Option<SocketAddr>; 2],
) -> impl Iterator<Item = (&'a Payload, SocketAddr)> + 'a {
    payloads.iter().flat_map(|payload| {
        destinations
            .iter()
            .copied()
            // Production filters absent and duplicate destinations, leaving a
            // zero lower size hint even when every destination is present.
            .filter_map(move |addr| addr.map(|addr| (payload, addr)))
    })
}

fn bench_collect(bencher: &mut Bencher, fec_sets: usize, destination_count: usize) {
    let payloads = make_payloads(fec_sets);
    let destinations = destinations(destination_count);
    bencher.iter(|| {
        let packets: Vec<(&Payload, SocketAddr)> =
            descriptors(black_box(payloads.as_slice()), black_box(&destinations)).collect();
        black_box(packets);
    });
}

fn bench_reused(bencher: &mut Bencher, fec_sets: usize, destination_count: usize) {
    let payloads = make_payloads(fec_sets);
    let destinations = destinations(destination_count);
    let mut packets = Vec::new();
    packets.extend(descriptors(&payloads, &destinations));
    bencher.iter(|| {
        packets.clear();
        packets.extend(descriptors(
            black_box(payloads.as_slice()),
            black_box(&destinations),
        ));
        black_box(&packets);
    });
}

macro_rules! descriptor_benchmarks {
    ($collect:ident, $reused:ident, $fec_sets:expr, $destinations:expr) => {
        fn $collect(bencher: &mut Bencher) {
            bench_collect(bencher, $fec_sets, $destinations);
        }

        fn $reused(bencher: &mut Bencher) {
            bench_reused(bencher, $fec_sets, $destinations);
        }
    };
}

// Production samples were closest to 19, 24, and 34 complete 64-shred FEC
// sets (1,216, 1,536, and 2,176 total data + coding shreds).
descriptor_benchmarks!(
    bench_collect_19_fec_0_dest,
    bench_reused_19_fec_0_dest,
    19,
    0
);
descriptor_benchmarks!(
    bench_collect_19_fec_1_dest,
    bench_reused_19_fec_1_dest,
    19,
    1
);
descriptor_benchmarks!(
    bench_collect_19_fec_2_dest,
    bench_reused_19_fec_2_dest,
    19,
    2
);
descriptor_benchmarks!(
    bench_collect_24_fec_0_dest,
    bench_reused_24_fec_0_dest,
    24,
    0
);
descriptor_benchmarks!(
    bench_collect_24_fec_1_dest,
    bench_reused_24_fec_1_dest,
    24,
    1
);
descriptor_benchmarks!(
    bench_collect_24_fec_2_dest,
    bench_reused_24_fec_2_dest,
    24,
    2
);
descriptor_benchmarks!(
    bench_collect_34_fec_0_dest,
    bench_reused_34_fec_0_dest,
    34,
    0
);
descriptor_benchmarks!(
    bench_collect_34_fec_1_dest,
    bench_reused_34_fec_1_dest,
    34,
    1
);
descriptor_benchmarks!(
    bench_collect_34_fec_2_dest,
    bench_reused_34_fec_2_dest,
    34,
    2
);

benchmark_group!(
    benches,
    bench_collect_19_fec_0_dest,
    bench_reused_19_fec_0_dest,
    bench_collect_19_fec_1_dest,
    bench_reused_19_fec_1_dest,
    bench_collect_19_fec_2_dest,
    bench_reused_19_fec_2_dest,
    bench_collect_24_fec_0_dest,
    bench_reused_24_fec_0_dest,
    bench_collect_24_fec_1_dest,
    bench_reused_24_fec_1_dest,
    bench_collect_24_fec_2_dest,
    bench_reused_24_fec_2_dest,
    bench_collect_34_fec_0_dest,
    bench_reused_34_fec_0_dest,
    bench_collect_34_fec_1_dest,
    bench_reused_34_fec_1_dest,
    bench_collect_34_fec_2_dest,
    bench_reused_34_fec_2_dest,
);
benchmark_main!(benches);
