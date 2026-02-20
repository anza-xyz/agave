#![allow(clippy::arithmetic_side_effects)]

use {
    bencher::{Bencher, benchmark_group, benchmark_main},
    rand::prelude::*,
    solana_perf::{
        deduper::{self, Deduper},
        packet::{PacketBatch, to_packet_batches},
    },
    std::time::Duration,
};

#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

const NUM: usize = 4096;

fn test_packet_with_size(size: usize, rng: &mut ThreadRng) -> Vec<u8> {
    // subtract 8 bytes because the length will get serialized as well
    (0..size.checked_sub(8).unwrap())
        .map(|_| rng.random())
        .collect()
}

fn do_bench_dedup_packets(b: &mut Bencher, mut batches: Vec<PacketBatch>) {
    // verify packets
    let mut rng = rand::rng();
    let mut deduper = Deduper::<2, [u8]>::new(&mut rng, /*num_bits:*/ 63_999_979);
    b.iter(|| {
        let _ans = deduper::dedup_packets_and_count_discards(&deduper, &mut batches);
        deduper.maybe_reset(
            &mut rng,
            0.001,                  // false_positive_rate
            Duration::from_secs(2), // reset_cycle
        );
        batches.iter_mut().for_each(|b| {
            b.iter_mut()
                .for_each(|mut p| p.meta_mut().set_discard(false))
        });
    });
}

fn bench_dedup_same_small_packets(b: &mut Bencher) {
    let mut rng = rand::rng();
    let small_packet = test_packet_with_size(128, &mut rng);

    let batches = to_packet_batches(
        &std::iter::repeat_n(small_packet, NUM).collect::<Vec<_>>(),
        128,
    );

    do_bench_dedup_packets(b, batches);
}

fn bench_dedup_same_big_packets(b: &mut Bencher) {
    let mut rng = rand::rng();
    let big_packet = test_packet_with_size(1024, &mut rng);

    let batches = to_packet_batches(
        &std::iter::repeat_n(big_packet, NUM).collect::<Vec<_>>(),
        128,
    );

    do_bench_dedup_packets(b, batches);
}

fn bench_dedup_diff_small_packets(b: &mut Bencher) {
    let mut rng = rand::rng();

    let batches = to_packet_batches(
        &(0..NUM)
            .map(|_| test_packet_with_size(128, &mut rng))
            .collect::<Vec<_>>(),
        128,
    );

    do_bench_dedup_packets(b, batches);
}

fn bench_dedup_diff_big_packets(b: &mut Bencher) {
    let mut rng = rand::rng();

    let batches = to_packet_batches(
        &(0..NUM)
            .map(|_| test_packet_with_size(1024, &mut rng))
            .collect::<Vec<_>>(),
        128,
    );

    do_bench_dedup_packets(b, batches);
}

fn bench_dedup_baseline(b: &mut Bencher) {
    let mut rng = rand::rng();

    let batches = to_packet_batches(
        &(0..0)
            .map(|_| test_packet_with_size(128, &mut rng))
            .collect::<Vec<_>>(),
        128,
    );

    do_bench_dedup_packets(b, batches);
}

fn bench_dedup_reset(b: &mut Bencher) {
    let mut rng = rand::rng();
    let mut deduper = Deduper::<2, [u8]>::new(&mut rng, /*num_bits:*/ 63_999_979);
    b.iter(|| {
        deduper.maybe_reset(
            &mut rng,
            0.001,                    // false_positive_rate
            Duration::from_millis(0), // reset_cycle
        );
    });
}

benchmark_group!(
    benches,
    bench_dedup_reset,
    bench_dedup_baseline,
    bench_dedup_diff_big_packets,
    bench_dedup_diff_small_packets,
    bench_dedup_same_big_packets,
    bench_dedup_same_small_packets
);
benchmark_main!(benches);
