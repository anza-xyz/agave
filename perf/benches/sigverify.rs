// TODO: simplify this bench and remove the batch_old methods.
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

fn bench_sigverify(
    b: &mut Bencher,
    mut batches: Vec<PacketBatch>,
    verify: fn(&rayon::ThreadPool, &mut [PacketBatch], bool, usize),
) {
    let threadpool = sigverify::threadpool_for_benches();
    let num_packets = sigverify::count_packets_in_batches(&batches);
    b.iter(|| {
        verify(&threadpool, &mut batches, false, num_packets);
    });
}

fn bench_sigverify_simple_old(b: &mut Bencher) {
    let tx = test_tx();
    let batches = to_packet_batches(&std::iter::repeat_n(tx, NUM).collect::<Vec<_>>(), 128);
    bench_sigverify(b, batches, sigverify::ed25519_verify_old_for_bench);
}

fn bench_sigverify_simple_new(b: &mut Bencher) {
    let tx = test_tx();
    let batches = to_packet_batches(&std::iter::repeat_n(tx, NUM).collect::<Vec<_>>(), 128);
    bench_sigverify(b, batches, sigverify::ed25519_verify_new_for_bench);
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

fn bench_sigverify_low_packets_small_batch_old(b: &mut Bencher) {
    let batches = gen_batches(false, 1, sigverify::VERIFY_PACKET_CHUNK_SIZE - 1);
    bench_sigverify(b, batches, sigverify::ed25519_verify_old_for_bench);
}

fn bench_sigverify_low_packets_small_batch_new(b: &mut Bencher) {
    let batches = gen_batches(false, 1, sigverify::VERIFY_PACKET_CHUNK_SIZE - 1);
    bench_sigverify(b, batches, sigverify::ed25519_verify_new_for_bench);
}

fn bench_sigverify_low_packets_large_batch_old(b: &mut Bencher) {
    let batches = gen_batches(
        false,
        LARGE_BATCH_PACKET_COUNT,
        sigverify::VERIFY_PACKET_CHUNK_SIZE - 1,
    );
    bench_sigverify(b, batches, sigverify::ed25519_verify_old_for_bench);
}

fn bench_sigverify_low_packets_large_batch_new(b: &mut Bencher) {
    let batches = gen_batches(
        false,
        LARGE_BATCH_PACKET_COUNT,
        sigverify::VERIFY_PACKET_CHUNK_SIZE - 1,
    );
    bench_sigverify(b, batches, sigverify::ed25519_verify_new_for_bench);
}

fn bench_sigverify_medium_packets_small_batch_old(b: &mut Bencher) {
    let batches = gen_batches(false, 1, sigverify::VERIFY_PACKET_CHUNK_SIZE * 8);
    bench_sigverify(b, batches, sigverify::ed25519_verify_old_for_bench);
}

fn bench_sigverify_medium_packets_small_batch_new(b: &mut Bencher) {
    let batches = gen_batches(false, 1, sigverify::VERIFY_PACKET_CHUNK_SIZE * 8);
    bench_sigverify(b, batches, sigverify::ed25519_verify_new_for_bench);
}

fn bench_sigverify_medium_packets_large_batch_old(b: &mut Bencher) {
    let batches = gen_batches(false, LARGE_BATCH_PACKET_COUNT, sigverify::VERIFY_PACKET_CHUNK_SIZE * 8);
    bench_sigverify(b, batches, sigverify::ed25519_verify_old_for_bench);
}

fn bench_sigverify_medium_packets_large_batch_new(b: &mut Bencher) {
    let batches = gen_batches(false, LARGE_BATCH_PACKET_COUNT, sigverify::VERIFY_PACKET_CHUNK_SIZE * 8);
    bench_sigverify(b, batches, sigverify::ed25519_verify_new_for_bench);
}

fn bench_sigverify_high_packets_small_batch_old(b: &mut Bencher) {
    let batches = gen_batches(false, 1, sigverify::VERIFY_PACKET_CHUNK_SIZE * 32);
    bench_sigverify(b, batches, sigverify::ed25519_verify_old_for_bench);
}

fn bench_sigverify_high_packets_small_batch_new(b: &mut Bencher) {
    let batches = gen_batches(false, 1, sigverify::VERIFY_PACKET_CHUNK_SIZE * 32);
    bench_sigverify(b, batches, sigverify::ed25519_verify_new_for_bench);
}

fn bench_sigverify_high_packets_large_batch_old(b: &mut Bencher) {
    let batches = gen_batches(
        false,
        LARGE_BATCH_PACKET_COUNT,
        sigverify::VERIFY_PACKET_CHUNK_SIZE * 32,
    );
    bench_sigverify(b, batches, sigverify::ed25519_verify_old_for_bench);
}

fn bench_sigverify_high_packets_large_batch_new(b: &mut Bencher) {
    let batches = gen_batches(
        false,
        LARGE_BATCH_PACKET_COUNT,
        sigverify::VERIFY_PACKET_CHUNK_SIZE * 32,
    );
    bench_sigverify(b, batches, sigverify::ed25519_verify_new_for_bench);
}

fn make_uneven_batches() -> Vec<PacketBatch> {
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
            let mut packet = BytesPacket::from_data(None, &tx).expect("serialize request");
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
    batches
}

fn bench_sigverify_uneven_old(b: &mut Bencher) {
    bench_sigverify(b, make_uneven_batches(), sigverify::ed25519_verify_old_for_bench);
}

fn bench_sigverify_uneven_new(b: &mut Bencher) {
    bench_sigverify(b, make_uneven_batches(), sigverify::ed25519_verify_new_for_bench);
}

benchmark_group!(
    benches,
    bench_sigverify_uneven_old,
    bench_sigverify_uneven_new,
    bench_sigverify_high_packets_large_batch_old,
    bench_sigverify_high_packets_large_batch_new,
    bench_sigverify_high_packets_small_batch_old,
    bench_sigverify_high_packets_small_batch_new,
    bench_sigverify_medium_packets_large_batch_old,
    bench_sigverify_medium_packets_large_batch_new,
    bench_sigverify_medium_packets_small_batch_old,
    bench_sigverify_medium_packets_small_batch_new,
    bench_sigverify_low_packets_large_batch_old,
    bench_sigverify_low_packets_large_batch_new,
    bench_sigverify_low_packets_small_batch_old,
    bench_sigverify_low_packets_small_batch_new,
    bench_sigverify_simple_old,
    bench_sigverify_simple_new
);
benchmark_main!(benches);
