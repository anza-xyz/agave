#![allow(clippy::arithmetic_side_effects)]

use {
    bencher::{benchmark_group, benchmark_main, Bencher},
    log::*,
    rand::{thread_rng, Rng},
    solana_perf::{
        packet::{to_packet_batches, BytesPacket, BytesPacketBatch, PacketBatch},
        recycler::Recycler,
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
    let tx = test_tx();
    let num_packets = NUM;

    // generate packet vector
    let mut batches = to_packet_batches(
        &std::iter::repeat_n(tx, num_packets).collect::<Vec<_>>(),
        128,
    );

    // verify packets
    b.iter(|| {
        sigverify::ed25519_verify(&mut batches, false, num_packets);
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
    let num_packets = sigverify::VERIFY_PACKET_CHUNK_SIZE - 1;
    let mut batches = gen_batches(false, 1, num_packets);
    b.iter(|| {
        sigverify::ed25519_verify(&mut batches, false, num_packets);
    })
}

fn bench_sigverify_low_packets_large_batch(b: &mut Bencher) {
    let num_packets = sigverify::VERIFY_PACKET_CHUNK_SIZE - 1;
    let mut batches = gen_batches(false, LARGE_BATCH_PACKET_COUNT, num_packets);
    b.iter(|| {
        sigverify::ed25519_verify(&mut batches, false, num_packets);
    })
}

fn bench_sigverify_medium_packets_small_batch(b: &mut Bencher) {
    let num_packets = sigverify::VERIFY_PACKET_CHUNK_SIZE * 8;
    let mut batches = gen_batches(false, 1, num_packets);
    b.iter(|| {
        sigverify::ed25519_verify(&mut batches, false, num_packets);
    })
}

fn bench_sigverify_medium_packets_large_batch(b: &mut Bencher) {
    let num_packets = sigverify::VERIFY_PACKET_CHUNK_SIZE * 8;
    let mut batches = gen_batches(false, LARGE_BATCH_PACKET_COUNT, num_packets);
    b.iter(|| {
        sigverify::ed25519_verify(&mut batches, false, num_packets);
    })
}

fn bench_sigverify_high_packets_small_batch(b: &mut Bencher) {
    let num_packets = sigverify::VERIFY_PACKET_CHUNK_SIZE * 32;
    let mut batches = gen_batches(false, 1, num_packets);
    b.iter(|| {
        sigverify::ed25519_verify(&mut batches, false, num_packets);
    })
}

fn bench_sigverify_high_packets_large_batch(b: &mut Bencher) {
    let num_packets = sigverify::VERIFY_PACKET_CHUNK_SIZE * 32;
    let mut batches = gen_batches(false, LARGE_BATCH_PACKET_COUNT, num_packets);
    // verify packets
    b.iter(|| {
        sigverify::ed25519_verify(&mut batches, false, num_packets);
    })
}

fn bench_sigverify_uneven(b: &mut Bencher) {
    agave_logger::setup();
    let simple_tx = test_tx();
    let multi_tx = test_multisig_tx();
    let mut tx;

    let num_packets = NUM * 50;
    let mut num_valid = 0;
    let mut current_packets = 0;
    // generate packet vector
    let mut batches = vec![];
    while current_packets < num_packets {
        let mut len: usize = thread_rng().gen_range(1..128);
        current_packets += len;
        if current_packets > num_packets {
            len -= current_packets - num_packets;
            current_packets = num_packets;
        }
        let mut batch = BytesPacketBatch::with_capacity(len);
        for _ in 0..len {
            if thread_rng().gen_ratio(1, 2) {
                tx = simple_tx.clone();
            } else {
                tx = multi_tx.clone();
            };
            let mut packet = BytesPacket::from_data(None, &tx).expect("serialize request");
            if thread_rng().gen_ratio((num_packets - NUM) as u32, num_packets as u32) {
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
        sigverify::ed25519_verify(&mut batches, false, num_packets);
    })
}

fn bench_get_offsets(b: &mut Bencher) {
    let tx = test_tx();

    // generate packet vector
    let mut batches = to_packet_batches(&std::iter::repeat_n(tx, 1024).collect::<Vec<_>>(), 1024);

    let recycler = Recycler::default();
    // verify packets
    b.iter(|| {
        let _ans = sigverify::generate_offsets(&mut batches, &recycler, false);
    })
}

benchmark_group!(
    benches,
    bench_get_offsets,
    bench_sigverify_uneven,
    bench_sigverify_high_packets_large_batch,
    bench_sigverify_high_packets_small_batch,
    bench_sigverify_medium_packets_large_batch,
    bench_sigverify_medium_packets_small_batch,
    bench_sigverify_low_packets_large_batch,
    bench_sigverify_low_packets_small_batch,
    bench_sigverify_simple
);
benchmark_main!(benches);
