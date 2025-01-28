#![feature(test)]

extern crate test;
use {
    rayon::ThreadPoolBuilder,
    solana_ledger::{
        shred::{Shred, ShredFlags, LEGACY_SHRED_DATA_CAPACITY},
        sigverify_shreds::sign_shreds_cpu,
    },
    solana_perf::packet::{Packet, PacketBatch},
    solana_rayon_threadlimit::get_thread_count,
    solana_sdk::signature::Keypair,
    test::Bencher,
};

const NUM_PACKETS: usize = 256;
const NUM_BATCHES: usize = 1;

#[bench]
fn bench_sigverify_shreds_sign_cpu(bencher: &mut Bencher) {
    let thread_pool = ThreadPoolBuilder::new()
        .num_threads(get_thread_count())
        .build()
        .unwrap();
    let mut packet_batch = PacketBatch::default();
    let slot = 0xdead_c0de;
    packet_batch.resize(NUM_PACKETS, Packet::default());
    for p in packet_batch.iter_mut() {
        let shred = Shred::new_from_data(
            slot,
            0xc0de,
            0xdead,
            &[5; LEGACY_SHRED_DATA_CAPACITY],
            ShredFlags::LAST_SHRED_IN_SLOT,
            1,
            2,
            0,
        );
        shred.copy_to_packet(p);
    }
    let mut batches = vec![packet_batch; NUM_BATCHES];
    let keypair = Keypair::new();
    bencher.iter(|| {
        sign_shreds_cpu(&thread_pool, &keypair, &mut batches);
    })
}
