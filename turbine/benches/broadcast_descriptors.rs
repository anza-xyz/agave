use {
    bencher::{Bencher, benchmark_group, benchmark_main},
    solana_ledger::shred::Payload,
    std::{hint::black_box, net::SocketAddr},
};

const SHREDS_PER_FEC_SET: usize = 64;
const PAYLOAD_SIZE: usize = 1_228;

#[derive(Clone, Copy)]
enum SendMode {
    Udp,
    Xdp,
}

fn make_payloads(fec_sets: usize) -> Vec<Payload> {
    let num_shreds = fec_sets
        .checked_mul(SHREDS_PER_FEC_SET)
        .expect("benchmark FEC shape fits usize");
    (0..num_shreds)
        .map(|_| Payload::from(vec![0; PAYLOAD_SIZE]))
        .collect()
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

fn indexed_descriptors(
    num_payloads: usize,
    destinations: &[Option<SocketAddr>; 2],
) -> impl Iterator<Item = (usize, SocketAddr)> + '_ {
    (0..num_payloads).flat_map(|shred_index| {
        destinations
            .iter()
            .copied()
            .filter_map(move |addr| addr.map(|addr| (shred_index, addr)))
    })
}

fn consume_direct(packets: &[(&Payload, SocketAddr)], mode: SendMode) {
    match mode {
        SendMode::Udp => {
            for &(payload, addr) in packets {
                black_box((payload.as_ref(), addr));
            }
        }
        SendMode::Xdp => {
            for &(payload, addr) in packets {
                black_box((payload.bytes.clone(), addr));
            }
        }
    }
}

fn consume_indexed(payloads: &[Payload], packets: &[(usize, SocketAddr)]) {
    for &(shred_index, addr) in packets {
        black_box((payloads[shred_index].as_ref(), addr));
    }
}

fn bench_collect(bencher: &mut Bencher, fec_sets: usize, destination_count: usize, mode: SendMode) {
    let payloads = make_payloads(fec_sets);
    let destinations = destinations(destination_count);
    bencher.iter(|| {
        let packets: Vec<(&Payload, SocketAddr)> =
            descriptors(black_box(payloads.as_slice()), black_box(&destinations)).collect();
        consume_direct(&packets, mode);
        black_box(&packets);
    });
}

fn bench_candidate(
    bencher: &mut Bencher,
    fec_sets: usize,
    destination_count: usize,
    mode: SendMode,
) {
    let payloads = make_payloads(fec_sets);
    let destinations = destinations(destination_count);
    match mode {
        SendMode::Udp => {
            // Retained rejected prototype: index resolution is shape-dependent,
            // so production keeps direct borrowed descriptors for UDP.
            let mut packets = Vec::new();
            packets.extend(indexed_descriptors(payloads.len(), &destinations));
            bencher.iter(|| {
                packets.clear();
                packets.extend(indexed_descriptors(
                    black_box(payloads.len()),
                    black_box(&destinations),
                ));
                consume_indexed(&payloads, &packets);
                black_box(&packets);
            });
        }
        SendMode::Xdp => {
            // Production XDP scratch owns the shallow Bytes handles which the
            // asynchronous sender already requires, then drains them in order.
            let mut packets = Vec::new();
            packets.extend(
                indexed_descriptors(payloads.len(), &destinations)
                    .map(|(shred_index, addr)| (payloads[shred_index].bytes.clone(), addr)),
            );
            bencher.iter(|| {
                packets.clear();
                packets.extend(
                    indexed_descriptors(black_box(payloads.len()), black_box(&destinations))
                        .map(|(shred_index, addr)| (payloads[shred_index].bytes.clone(), addr)),
                );
                for packet in packets.drain(..) {
                    black_box(packet);
                }
            });
        }
    }
}

macro_rules! descriptor_benchmarks {
    ($collect:ident, $candidate:ident, $fec_sets:expr, $destinations:expr, $mode:expr) => {
        fn $collect(bencher: &mut Bencher) {
            bench_collect(bencher, $fec_sets, $destinations, $mode);
        }

        fn $candidate(bencher: &mut Bencher) {
            bench_candidate(bencher, $fec_sets, $destinations, $mode);
        }
    };
}

macro_rules! descriptor_shape_benchmarks {
    (
        $collect_udp:ident,
        $indexed_udp:ident,
        $collect_xdp:ident,
        $reused_bytes_xdp:ident,
        $fec_sets:expr,
        $destinations:expr
    ) => {
        descriptor_benchmarks!(
            $collect_udp,
            $indexed_udp,
            $fec_sets,
            $destinations,
            SendMode::Udp
        );
        descriptor_benchmarks!(
            $collect_xdp,
            $reused_bytes_xdp,
            $fec_sets,
            $destinations,
            SendMode::Xdp
        );
    };
}

// Production samples were closest to 19, 24, and 34 complete 64-shred FEC
// sets (1,216, 1,536, and 2,176 total data + coding shreds).
descriptor_shape_benchmarks!(
    bench_collect_udp_19_fec_0_dest,
    bench_indexed_udp_19_fec_0_dest,
    bench_collect_xdp_19_fec_0_dest,
    bench_reused_bytes_xdp_19_fec_0_dest,
    19,
    0
);
descriptor_shape_benchmarks!(
    bench_collect_udp_19_fec_1_dest,
    bench_indexed_udp_19_fec_1_dest,
    bench_collect_xdp_19_fec_1_dest,
    bench_reused_bytes_xdp_19_fec_1_dest,
    19,
    1
);
descriptor_shape_benchmarks!(
    bench_collect_udp_19_fec_2_dest,
    bench_indexed_udp_19_fec_2_dest,
    bench_collect_xdp_19_fec_2_dest,
    bench_reused_bytes_xdp_19_fec_2_dest,
    19,
    2
);
descriptor_shape_benchmarks!(
    bench_collect_udp_24_fec_0_dest,
    bench_indexed_udp_24_fec_0_dest,
    bench_collect_xdp_24_fec_0_dest,
    bench_reused_bytes_xdp_24_fec_0_dest,
    24,
    0
);
descriptor_shape_benchmarks!(
    bench_collect_udp_24_fec_1_dest,
    bench_indexed_udp_24_fec_1_dest,
    bench_collect_xdp_24_fec_1_dest,
    bench_reused_bytes_xdp_24_fec_1_dest,
    24,
    1
);
descriptor_shape_benchmarks!(
    bench_collect_udp_24_fec_2_dest,
    bench_indexed_udp_24_fec_2_dest,
    bench_collect_xdp_24_fec_2_dest,
    bench_reused_bytes_xdp_24_fec_2_dest,
    24,
    2
);
descriptor_shape_benchmarks!(
    bench_collect_udp_34_fec_0_dest,
    bench_indexed_udp_34_fec_0_dest,
    bench_collect_xdp_34_fec_0_dest,
    bench_reused_bytes_xdp_34_fec_0_dest,
    34,
    0
);
descriptor_shape_benchmarks!(
    bench_collect_udp_34_fec_1_dest,
    bench_indexed_udp_34_fec_1_dest,
    bench_collect_xdp_34_fec_1_dest,
    bench_reused_bytes_xdp_34_fec_1_dest,
    34,
    1
);
descriptor_shape_benchmarks!(
    bench_collect_udp_34_fec_2_dest,
    bench_indexed_udp_34_fec_2_dest,
    bench_collect_xdp_34_fec_2_dest,
    bench_reused_bytes_xdp_34_fec_2_dest,
    34,
    2
);

benchmark_group!(
    benches,
    bench_collect_udp_19_fec_0_dest,
    bench_indexed_udp_19_fec_0_dest,
    bench_collect_xdp_19_fec_0_dest,
    bench_reused_bytes_xdp_19_fec_0_dest,
    bench_collect_udp_19_fec_1_dest,
    bench_indexed_udp_19_fec_1_dest,
    bench_collect_xdp_19_fec_1_dest,
    bench_reused_bytes_xdp_19_fec_1_dest,
    bench_collect_udp_19_fec_2_dest,
    bench_indexed_udp_19_fec_2_dest,
    bench_collect_xdp_19_fec_2_dest,
    bench_reused_bytes_xdp_19_fec_2_dest,
    bench_collect_udp_24_fec_0_dest,
    bench_indexed_udp_24_fec_0_dest,
    bench_collect_xdp_24_fec_0_dest,
    bench_reused_bytes_xdp_24_fec_0_dest,
    bench_collect_udp_24_fec_1_dest,
    bench_indexed_udp_24_fec_1_dest,
    bench_collect_xdp_24_fec_1_dest,
    bench_reused_bytes_xdp_24_fec_1_dest,
    bench_collect_udp_24_fec_2_dest,
    bench_indexed_udp_24_fec_2_dest,
    bench_collect_xdp_24_fec_2_dest,
    bench_reused_bytes_xdp_24_fec_2_dest,
    bench_collect_udp_34_fec_0_dest,
    bench_indexed_udp_34_fec_0_dest,
    bench_collect_xdp_34_fec_0_dest,
    bench_reused_bytes_xdp_34_fec_0_dest,
    bench_collect_udp_34_fec_1_dest,
    bench_indexed_udp_34_fec_1_dest,
    bench_collect_xdp_34_fec_1_dest,
    bench_reused_bytes_xdp_34_fec_1_dest,
    bench_collect_udp_34_fec_2_dest,
    bench_indexed_udp_34_fec_2_dest,
    bench_collect_xdp_34_fec_2_dest,
    bench_reused_bytes_xdp_34_fec_2_dest,
);
benchmark_main!(benches);
