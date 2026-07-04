#![allow(clippy::arithmetic_side_effects)]

use {
    bencher::{Bencher, benchmark_group, benchmark_main},
    log::*,
    rand::Rng,
    solana_perf::{
        packet::{BytesPacket, BytesPacketBatch, PacketBatch, to_packet_batches},
        sigverify,
        test_tx::{test_multisig_tx, test_tx},
    },
};

#[cfg(not(any(target_env = "msvc", target_os = "freebsd")))]
#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

const NUM: usize = 256;
const LARGE_BATCH_PACKET_COUNT: usize = 128;

fn bench_sigverify_simple(b: &mut Bencher) {
    let threadpool = sigverify::threadpool_for_benches();
    let tx = test_tx();
    let num_packets = NUM;

    // generate packet vector
    let mut batches = to_packet_batches(
        &std::iter::repeat_n(tx, num_packets).collect::<Vec<_>>(),
        128,
    );

    // verify packets
    b.iter(|| {
        sigverify::ed25519_verify(&threadpool, &mut batches, false, num_packets, false);
    })
}

fn gen_batches(
    use_same_tx: bool,
    packets_per_batch: usize,
    total_packets: usize,
) -> Vec<PacketBatch> {
    if use_same_tx {
        let tx = test_tx();
        to_packet_batches(&vec![tx; total_packets], packets_per_batch)
    } else {
        let txs: Vec<_> = std::iter::repeat_with(test_tx)
            .take(total_packets)
            .collect();
        to_packet_batches(&txs, packets_per_batch)
    }
}

fn bench_sigverify_low_packets_small_batch(b: &mut Bencher) {
    let threadpool = sigverify::threadpool_for_benches();
    let num_packets = sigverify::VERIFY_PACKET_CHUNK_SIZE - 1;
    let mut batches = gen_batches(false, 1, num_packets);
    b.iter(|| {
        sigverify::ed25519_verify(&threadpool, &mut batches, false, num_packets, false);
    })
}

fn bench_sigverify_low_packets_large_batch(b: &mut Bencher) {
    let threadpool = sigverify::threadpool_for_benches();
    let num_packets = sigverify::VERIFY_PACKET_CHUNK_SIZE - 1;
    let mut batches = gen_batches(false, LARGE_BATCH_PACKET_COUNT, num_packets);
    b.iter(|| {
        sigverify::ed25519_verify(&threadpool, &mut batches, false, num_packets, false);
    })
}

fn bench_sigverify_medium_packets_small_batch(b: &mut Bencher) {
    let threadpool = sigverify::threadpool_for_benches();
    let num_packets = sigverify::VERIFY_PACKET_CHUNK_SIZE * 8;
    let mut batches = gen_batches(false, 1, num_packets);
    b.iter(|| {
        sigverify::ed25519_verify(&threadpool, &mut batches, false, num_packets, false);
    })
}

fn bench_sigverify_medium_packets_large_batch(b: &mut Bencher) {
    let threadpool = sigverify::threadpool_for_benches();
    let num_packets = sigverify::VERIFY_PACKET_CHUNK_SIZE * 8;
    let mut batches = gen_batches(false, LARGE_BATCH_PACKET_COUNT, num_packets);
    b.iter(|| {
        sigverify::ed25519_verify(&threadpool, &mut batches, false, num_packets, false);
    })
}

fn bench_sigverify_high_packets_small_batch(b: &mut Bencher) {
    let threadpool = sigverify::threadpool_for_benches();
    let num_packets = sigverify::VERIFY_PACKET_CHUNK_SIZE * 32;
    let mut batches = gen_batches(false, 1, num_packets);
    b.iter(|| {
        sigverify::ed25519_verify(&threadpool, &mut batches, false, num_packets, false);
    })
}

fn bench_sigverify_high_packets_large_batch(b: &mut Bencher) {
    let threadpool = sigverify::threadpool_for_benches();
    let num_packets = sigverify::VERIFY_PACKET_CHUNK_SIZE * 32;
    let mut batches = gen_batches(false, LARGE_BATCH_PACKET_COUNT, num_packets);
    // verify packets
    b.iter(|| {
        sigverify::ed25519_verify(&threadpool, &mut batches, false, num_packets, false);
    })
}

/// Builds one multi-packet batch for the serial sigverify path.
fn gen_single_batch(use_same_tx: bool, num_packets: usize) -> PacketBatch {
    let mut packet_batch = BytesPacketBatch::with_capacity(num_packets);
    if use_same_tx {
        let packet = BytesPacket::from_data(test_tx()).expect("serialize request");
        for _ in 0..num_packets {
            packet_batch.push(packet.clone());
        }
    } else {
        for _ in 0..num_packets {
            packet_batch.push(BytesPacket::from_data(test_tx()).expect("serialize request"));
        }
    }
    PacketBatch::from(packet_batch)
}

/// Builds one-packet batches, matching QUIC packets drained into one serial
/// verify call.
fn gen_drained_singles(num_batches: usize) -> Vec<PacketBatch> {
    (0..num_batches)
        .map(|_| PacketBatch::Single(BytesPacket::from_data(test_tx()).expect("serialize")))
        .collect()
}

fn bench_sigverify_serial_single_packet(b: &mut Bencher) {
    let mut batches = gen_drained_singles(1);
    b.iter(|| {
        sigverify::ed25519_verify_serial(&mut batches, false, false);
    })
}

fn bench_sigverify_serial_drained_singles_8(b: &mut Bencher) {
    // Current worker drain cap.
    let mut batches = gen_drained_singles(8);
    b.iter(|| {
        sigverify::ed25519_verify_serial(&mut batches, false, false);
    })
}

fn bench_sigverify_serial_drained_singles_16(b: &mut Bencher) {
    let mut batches = gen_drained_singles(16);
    b.iter(|| {
        sigverify::ed25519_verify_serial(&mut batches, false, false);
    })
}

fn bench_sigverify_serial_drained_singles_64(b: &mut Bencher) {
    let mut batches = gen_drained_singles(64);
    b.iter(|| {
        sigverify::ed25519_verify_serial(&mut batches, false, false);
    })
}

fn bench_sigverify_serial_drained_singles_256(b: &mut Bencher) {
    let mut batches = gen_drained_singles(256);
    b.iter(|| {
        sigverify::ed25519_verify_serial(&mut batches, false, false);
    })
}

fn bench_sigverify_serial_drained_singles_512(b: &mut Bencher) {
    let mut batches = gen_drained_singles(512);
    b.iter(|| {
        sigverify::ed25519_verify_serial(&mut batches, false, false);
    })
}

fn bench_sigverify_serial_batch_2(b: &mut Bencher) {
    let mut batch = gen_single_batch(false, 2);
    b.iter(|| {
        sigverify::ed25519_verify_serial(std::slice::from_mut(&mut batch), false, false);
    })
}

fn bench_sigverify_serial_batch_4(b: &mut Bencher) {
    let mut batch = gen_single_batch(false, 4);
    b.iter(|| {
        sigverify::ed25519_verify_serial(std::slice::from_mut(&mut batch), false, false);
    })
}

fn bench_sigverify_serial_batch_8(b: &mut Bencher) {
    let mut batch = gen_single_batch(false, 8);
    b.iter(|| {
        sigverify::ed25519_verify_serial(std::slice::from_mut(&mut batch), false, false);
    })
}

fn bench_sigverify_serial_batch_16(b: &mut Bencher) {
    let mut batch = gen_single_batch(false, 16);
    b.iter(|| {
        sigverify::ed25519_verify_serial(std::slice::from_mut(&mut batch), false, false);
    })
}

fn bench_sigverify_serial_batch_32(b: &mut Bencher) {
    let mut batch = gen_single_batch(false, 32);
    b.iter(|| {
        sigverify::ed25519_verify_serial(std::slice::from_mut(&mut batch), false, false);
    })
}

fn bench_sigverify_serial_batch_128(b: &mut Bencher) {
    let mut batch = gen_single_batch(false, LARGE_BATCH_PACKET_COUNT);
    b.iter(|| {
        sigverify::ed25519_verify_serial(std::slice::from_mut(&mut batch), false, false);
    })
}

fn bench_sigverify_serial_batch_512(b: &mut Bencher) {
    let mut batch = gen_single_batch(false, 512);
    b.iter(|| {
        sigverify::ed25519_verify_serial(std::slice::from_mut(&mut batch), false, false);
    })
}

fn bench_sigverify_serial_batch_uneven(b: &mut Bencher) {
    let simple_tx = test_tx();
    let multi_tx = test_multisig_tx();
    let num_packets = LARGE_BATCH_PACKET_COUNT;
    let mut packet_batch = BytesPacketBatch::with_capacity(num_packets);
    for i in 0..num_packets {
        let tx = if i % 2 == 0 {
            simple_tx.clone()
        } else {
            multi_tx.clone()
        };
        packet_batch.push(BytesPacket::from_data(&tx).expect("serialize request"));
    }
    let mut batch = PacketBatch::from(packet_batch);
    b.iter(|| {
        sigverify::ed25519_verify_serial(std::slice::from_mut(&mut batch), false, false);
    })
}

fn bench_sigverify_uneven(b: &mut Bencher) {
    agave_logger::setup();
    let threadpool = sigverify::threadpool_for_benches();
    let simple_tx = test_tx();
    let multi_tx = test_multisig_tx();
    let mut tx;

    let num_packets = NUM * 50;
    let mut num_valid = 0;
    let mut current_packets = 0;
    // generate packet vector
    let mut batches = vec![];
    while current_packets < num_packets {
        let mut len: usize = rand::rng().random_range(1..128);
        current_packets += len;
        if current_packets > num_packets {
            len -= current_packets - num_packets;
            current_packets = num_packets;
        }
        let mut batch = BytesPacketBatch::with_capacity(len);
        for _ in 0..len {
            if rand::rng().random_ratio(1, 2) {
                tx = simple_tx.clone();
            } else {
                tx = multi_tx.clone();
            };
            let mut packet = BytesPacket::from_data(&tx).expect("serialize request");
            if rand::rng().random_ratio((num_packets - NUM) as u32, num_packets as u32) {
                packet.meta_mut().set_discard(true);
            } else {
                num_valid += 1;
            }
            batch.push(packet);
        }
        batches.push(PacketBatch::from(batch));
    }
    info!("num_packets: {num_packets} valid: {num_valid}");

    // verify packets
    b.iter(|| {
        sigverify::ed25519_verify(&threadpool, &mut batches, false, num_packets, false);
    })
}

benchmark_group!(
    benches,
    bench_sigverify_uneven,
    bench_sigverify_high_packets_large_batch,
    bench_sigverify_high_packets_small_batch,
    bench_sigverify_medium_packets_large_batch,
    bench_sigverify_medium_packets_small_batch,
    bench_sigverify_low_packets_large_batch,
    bench_sigverify_low_packets_small_batch,
    bench_sigverify_simple,
    bench_sigverify_serial_single_packet,
    bench_sigverify_serial_drained_singles_8,
    bench_sigverify_serial_drained_singles_16,
    bench_sigverify_serial_drained_singles_64,
    bench_sigverify_serial_drained_singles_256,
    bench_sigverify_serial_drained_singles_512,
    bench_sigverify_serial_batch_2,
    bench_sigverify_serial_batch_4,
    bench_sigverify_serial_batch_8,
    bench_sigverify_serial_batch_16,
    bench_sigverify_serial_batch_32,
    bench_sigverify_serial_batch_128,
    bench_sigverify_serial_batch_512,
    bench_sigverify_serial_batch_uneven
);
benchmark_main!(benches);
