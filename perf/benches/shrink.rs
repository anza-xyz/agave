#![allow(clippy::arithmetic_side_effects)]
#![feature(test)]

extern crate test;

use {
    rand::prelude::*,
    solana_perf::{
        packet::{to_packet_batches, PacketBatch, PACKETS_PER_BATCH},
        sigverify,
    },
    std::iter,
    test::Bencher,
};

#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

const NUM_PACKETS: usize = 1024 * 4;

fn test_packet_with_size(size: usize, rng: &mut ThreadRng) -> Vec<u8> {
    // subtract 8 bytes because the length will get serialized as well
    (0..size.checked_sub(8).unwrap())
        .map(|_| rng.gen())
        .collect()
}

fn do_bench_shrink_packets(bencher: &mut Bencher, mut batches: Vec<PacketBatch>) {
    let mut batches = iter::repeat_with(|| {
        batches.iter_mut().for_each(|b| {
            b.iter_mut()
                .for_each(|mut p| p.meta_mut().set_discard(thread_rng().gen()))
        });
        batches.clone()
    })
    .take(32)
    // Collect the shuffled batches to make sure that the benchmark iteration
    // doesn't spend cycles on memcopies and discarding packets.
    .collect::<Vec<_>>()
    .into_iter()
    .cycle();
    bencher.iter(|| {
        let batches = batches.next().unwrap();
        // verify packets
        sigverify::shrink_batches(batches);
    });
}

#[bench]
#[ignore]
fn bench_shrink_diff_small_packets(bencher: &mut Bencher) {
    let mut rng = rand::thread_rng();

    let batches = to_packet_batches(
        &(0..NUM_PACKETS)
            .map(|_| test_packet_with_size(128, &mut rng))
            .collect::<Vec<_>>(),
        PACKETS_PER_BATCH,
    );

    do_bench_shrink_packets(bencher, batches);
}

#[bench]
#[ignore]
fn bench_shrink_diff_big_packets(bencher: &mut Bencher) {
    let mut rng = rand::thread_rng();

    let batches = to_packet_batches(
        &(0..NUM_PACKETS)
            .map(|_| test_packet_with_size(1024, &mut rng))
            .collect::<Vec<_>>(),
        PACKETS_PER_BATCH,
    );

    do_bench_shrink_packets(bencher, batches);
}

#[bench]
#[ignore]
fn bench_shrink_count_packets(bencher: &mut Bencher) {
    let mut rng = rand::thread_rng();

    let mut batches = to_packet_batches(
        &(0..NUM_PACKETS)
            .map(|_| test_packet_with_size(128, &mut rng))
            .collect::<Vec<_>>(),
        PACKETS_PER_BATCH,
    );
    batches.iter_mut().for_each(|b| {
        b.iter_mut()
            .for_each(|mut p| p.meta_mut().set_discard(thread_rng().gen()))
    });

    bencher.iter(|| {
        let _ = sigverify::count_valid_packets(&batches);
    });
}
