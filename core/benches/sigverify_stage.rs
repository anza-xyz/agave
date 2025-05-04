#![allow(clippy::arithmetic_side_effects)]

extern crate solana_core;

use {
    criterion::{criterion_group, criterion_main, black_box, Criterion},
    crossbeam_channel::unbounded,
    log::*,
    rand::{
        distributions::{Distribution, Uniform},
        thread_rng, Rng,
    },
    solana_core::{
        banking_trace::BankingTracer,
        sigverify::TransactionSigVerifier,
        sigverify_stage::{SigVerifier, SigVerifyStage},
    },
    solana_measure::measure::Measure,
    solana_perf::{
        packet::{to_packet_batches, PacketBatch},
        test_tx::test_tx,
    },
    solana_sdk::{
        hash::Hash,
        signature::{Keypair, Signer},
        system_transaction,
    },
    std::time::{Duration, Instant},
};

#[allow(dead_code)]
fn run_bench_packet_discard(num_ips: usize, c: &mut Criterion) {
    solana_logger::setup();
    let len = 30 * 1000;
    let chunk_size = 1024;
    let tx = test_tx();
    let mut batches = to_packet_batches(&vec![tx; len], chunk_size);

    let mut total = 0;

    let ips: Vec<_> = (0..num_ips)
        .map(|_| {
            let mut addr = [0u16; 8];
            thread_rng().fill(&mut addr);
            std::net::IpAddr::from(addr)
        })
        .collect();

    for batch in batches.iter_mut() {
        total += batch.len();
        for p in batch.iter_mut() {
            let ip_index = thread_rng().gen_range(0..ips.len());
            p.meta_mut().addr = ips[ip_index];
        }
    }
    info!("total packets: {}", total);

    c.bench_function("discard_excess_packets", |b| {
        b.iter( || {
            SigVerifyStage::discard_excess_packets(&mut batches, 10_000);
            let mut num_packets = 0;
            for batch in batches.iter_mut() {
                for p in batch.iter_mut() {
                    if !p.meta().discard() {
                        num_packets += 1;
                    }
                    p.meta_mut().set_discard(false);
                }
            }
            assert_eq!(num_packets, 10_000);
        })
    });
}

#[allow(dead_code)]
fn bench_packet_discard_many_senders(c: &mut Criterion) {
    run_bench_packet_discard(1000, c);
}

#[allow(dead_code)]
fn bench_packet_discard_single_sender(c: &mut Criterion) {
    run_bench_packet_discard(1, c);
}

#[allow(dead_code)]
fn bench_packet_discard_mixed_senders(c: &mut Criterion) {
    const SIZE: usize = 30 * 1000;
    const CHUNK_SIZE: usize = 1024;
    fn new_rand_addr<R: Rng>(rng: &mut R) -> std::net::IpAddr {
        let mut addr = [0u16; 8];
        rng.fill(&mut addr);
        std::net::IpAddr::from(addr)
    }
    let mut rng = thread_rng();
    let mut batches = to_packet_batches(&vec![test_tx(); SIZE], CHUNK_SIZE);
    let spam_addr = new_rand_addr(&mut rng);
    for batch in batches.iter_mut() {
        for packet in batch.iter_mut() {
            // One spam address, ~1000 unique addresses.
            packet.meta_mut().addr = if rng.gen_ratio(1, 30) {
                new_rand_addr(&mut rng)
            } else {
                spam_addr
            }
        }
    }
    c.bench_function("discard_excess_packets", |b| {
        b.iter( || {
            SigVerifyStage::discard_excess_packets(&mut batches, 10_000);
            let mut num_packets = 0;
            for batch in batches.iter_mut() {
                for packet in batch.iter_mut() {
                    if !packet.meta().discard() {
                        num_packets += 1;
                    }
                    packet.meta_mut().set_discard(false);
                }
            }
            assert_eq!(num_packets, 10_000);
        })
    });
}

#[allow(dead_code)]
fn gen_batches(use_same_tx: bool) -> Vec<PacketBatch> {
    let len = 4096;
    let chunk_size = 1024;
    if use_same_tx {
        let tx = test_tx();
        to_packet_batches(&vec![tx; len], chunk_size)
    } else {
        let from_keypair = Keypair::new();
        let to_keypair = Keypair::new();
        let txs: Vec<_> = (0..len)
            .map(|_| {
                let amount = thread_rng().gen();
                system_transaction::transfer(
                    &from_keypair,
                    &to_keypair.pubkey(),
                    amount,
                    Hash::default(),
                )
            })
            .collect();
        to_packet_batches(&txs, chunk_size)
    }
}

#[allow(dead_code)]
fn bench_sigverify_stage_with_same_tx(c: &mut Criterion) {
    bench_sigverify_stage(c, true)
}

#[allow(dead_code)]
fn bench_sigverify_stage_without_same_tx(c: &mut Criterion) {
    bench_sigverify_stage(c, false)
}

#[allow(dead_code)]
fn bench_sigverify_stage(c: &mut Criterion, use_same_tx: bool) {
    solana_logger::setup();
    trace!("start");
    let (packet_s, packet_r) = unbounded();
    let (verified_s, verified_r) = BankingTracer::channel_for_test();
    let verifier = TransactionSigVerifier::new(verified_s, None);
    let stage = SigVerifyStage::new(packet_r, verifier, "solSigVerBench", "bench");

    c.bench_function("sigverify_stage", |b| {
        b.iter( || {
            let now = Instant::now();
            let batches = gen_batches(use_same_tx);
            trace!(
            "starting... generation took: {} ms batches: {}",
            now.elapsed().as_millis(),
            batches.len()
        );

        let mut sent_len = 0;
        for batch in batches.into_iter() {
            sent_len += batch.len();
            packet_s.send(batch).unwrap();
        }
        let mut received = 0;
        let expected = if use_same_tx { 1 } else { sent_len };
        trace!("sent: {}, expected: {}", sent_len, expected);
        loop {
            if let Ok(verifieds) = verified_r.recv_timeout(Duration::from_millis(10)) {
                received += verifieds.iter().map(|batch| batch.len()).sum::<usize>();
                black_box(verifieds);
                if received >= expected {
                    break;
                }
            }
            trace!("received: {}", received);
        }
        })
    });
    // This will wait for all packets to make it through sigverify.
    stage.join().unwrap();
}

fn prepare_batches(discard_factor: i32) -> (Vec<PacketBatch>, usize) {
    let len = 10_000; // max batch size
    let chunk_size = 1024;

    let from_keypair = Keypair::new();
    let to_keypair = Keypair::new();

    let txs: Vec<_> = (0..len)
        .map(|_| {
            let amount = thread_rng().gen();
            system_transaction::transfer(
                &from_keypair,
                &to_keypair.pubkey(),
                amount,
                Hash::default(),
            )
        })
        .collect();
    let mut batches = to_packet_batches(&txs, chunk_size);

    let mut rng = rand::thread_rng();
    let die = Uniform::<i32>::from(1..100);

    let mut c = 0;
    batches.iter_mut().for_each(|batch| {
        batch.iter_mut().for_each(|p| {
            let throw = die.sample(&mut rng);
            if throw < discard_factor {
                p.meta_mut().set_discard(true);
                c += 1;
            }
        })
    });
    (batches, len - c)
}

fn bench_shrink_sigverify_stage_core(c: &mut Criterion, discard_factor: i32) {
    let (batches0, num_valid_packets) = prepare_batches(discard_factor);
    let (verified_s, _verified_r) = BankingTracer::channel_for_test();
    let verifier = TransactionSigVerifier::new(verified_s, None);

    let mut total_shrink_time = 0;
    let mut total_verify_time = 0;
    let mut count = 0;

    c.bench_function("shrink_sigverify_stage", |b| {
        b.iter(|| {
            let mut batches = batches0.clone();
            let (pre_shrink_time_us, _pre_shrink_total) =
                SigVerifyStage::maybe_shrink_batches(&mut batches);
            let mut verify_time = Measure::start("sigverify_batch_time");
            let _batches = verifier.verify_batches(batches, num_valid_packets);
            verify_time.stop();

            count += 1;
            total_shrink_time += pre_shrink_time_us;
            total_verify_time += verify_time.as_us();
        })
    });

    error!(
        "bsv, {}, {}, {}",
        discard_factor,
        (total_shrink_time as f64) / (count as f64),
        (total_verify_time as f64) / (count as f64),
    );
}

macro_rules! gen_shrink_sigverify_bench {
    ($i:ident, $n:literal) => {
        fn $i(c: &mut Criterion) {
            bench_shrink_sigverify_stage_core(c, $n);
        }
    };
}

gen_shrink_sigverify_bench!(bsv_0, 0);
gen_shrink_sigverify_bench!(bsv_10, 10);
gen_shrink_sigverify_bench!(bsv_20, 20);
gen_shrink_sigverify_bench!(bsv_30, 30);
gen_shrink_sigverify_bench!(bsv_40, 40);
gen_shrink_sigverify_bench!(bsv_50, 50);
gen_shrink_sigverify_bench!(bsv_60, 60);
gen_shrink_sigverify_bench!(bsv_70, 70);
gen_shrink_sigverify_bench!(bsv_80, 80);
gen_shrink_sigverify_bench!(bsv_90, 90);

criterion_group!(
    name = benches;
    config = Criterion::default();
    targets = 
        
        bench_packet_discard_many_senders,
        bench_packet_discard_single_sender,
        bench_packet_discard_mixed_senders,
        bench_sigverify_stage_with_same_tx,
        bench_sigverify_stage_without_same_tx,
        bsv_0, bsv_10, bsv_20, bsv_30, bsv_40, bsv_50, bsv_60, bsv_70, bsv_80, bsv_90,
);

criterion_main!(benches);