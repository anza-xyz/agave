#![allow(clippy::arithmetic_side_effects)]

use {
    bencher::{Bencher, TDynBenchFn, TestDesc, TestDescAndFn, TestFn, benchmark_main},
    crossbeam_channel::bounded,
    log::*,
    rand::{
        Rng,
        distr::{Distribution, Uniform},
        rng,
    },
    solana_core::{banking_trace::BankingTracer, sigverify_stage::SigVerifyStage},
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_measure::measure::Measure,
    solana_perf::{
        packet::{BytesPacket, PacketBatch, to_packet_batches},
        sigverify,
        test_tx::test_tx,
    },
    solana_runtime::{bank::Bank, genesis_utils::create_genesis_config},
    solana_signer::Signer,
    solana_system_transaction as system_transaction,
    std::{
        borrow::Cow,
        hint::black_box,
        num::NonZeroUsize,
        sync::Arc,
        time::{Duration, Instant},
    },
};

/// Orphan rules workaround that allows for implementation of `TDynBenchFn`.
struct Bench<T>(T);

impl<T> TDynBenchFn for Bench<T>
where
    T: Fn(&mut Bencher) + Send,
{
    fn run(&self, harness: &mut Bencher) {
        (self.0)(harness)
    }
}

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
                let amount = rng().random();
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

fn bench_sigverify_stage_with_same_tx(bencher: &mut Bencher) {
    bench_sigverify_stage(bencher, true)
}

fn bench_sigverify_stage_without_same_tx(bencher: &mut Bencher) {
    bench_sigverify_stage(bencher, false)
}

fn bench_sigverify_stage(bencher: &mut Bencher, use_same_tx: bool) {
    agave_logger::setup();
    trace!("start");
    let (_bank, bank_forks) =
        Bank::new_with_bank_forks_for_tests(&create_genesis_config(1).genesis_config);
    let sharable_banks = bank_forks.read().unwrap().sharable_banks();
    let (packet_s, packet_r) = bounded(1024);
    let (vote_packet_s, vote_packet_r) = bounded(1024);
    let (verified_s, verified_r) = BankingTracer::channel_for_test();
    let (tpu_vote_s, _tpu_vote_r) = BankingTracer::channel_for_test();
    let (forward_stage_s, _forward_stage_r) = bounded(1024);
    let (stage, gossip_sigverify_handle) = SigVerifyStage::new(
        packet_r,
        vote_packet_r,
        verified_s,
        tpu_vote_s,
        forward_stage_s,
        NonZeroUsize::new(sigverify::threadpool_for_benches().current_num_threads()).unwrap(),
        false,
        sharable_banks,
        None,
    );
    let packet_s = packet_s;
    let packet_s_for_bench = packet_s.clone();
    let verified_r_for_bench = verified_r.clone();

    bencher.iter(move || {
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
            packet_s_for_bench.send(batch).unwrap();
        }
        let expected = if use_same_tx { 1 } else { sent_len };
        trace!("sent: {sent_len}, expected: {expected}");
        let mut verified = 0;
        while verified < expected {
            verified += verified_r_for_bench
                .recv()
                .unwrap()
                .iter()
                .map(|batch| {
                    batch
                        .iter()
                        .filter(|packet| !packet.meta().discard())
                        .count()
                })
                .sum::<usize>();
        }
        trace!("received: {verified}");
    });
    // This will wait for all packets to make it through sigverify.
    drop(packet_s);
    drop(vote_packet_s);
    drop(gossip_sigverify_handle);
    stage.join().unwrap();
}

/// Generates distinct single-packet batches, matching QUIC's input shape.
/// Distinct transactions avoid Deduper drops in the benchmark.
fn gen_single_packet_batches(
    from_keypair: &Keypair,
    to_keypair: &Keypair,
    count: usize,
) -> Vec<PacketBatch> {
    (0..count)
        .map(|_| {
            let amount = rng().random();
            let tx = system_transaction::transfer(
                from_keypair,
                &to_keypair.pubkey(),
                amount,
                Hash::default(),
            );
            PacketBatch::Single(BytesPacket::from_data(tx).expect("serialize request"))
        })
        .collect()
}

/// Measures signing and serialization cost without channel or sigverify work.
fn bench_gen_single_packet_batches(bencher: &mut Bencher) {
    let from_keypair = Keypair::new();
    let to_keypair = Keypair::new();
    bencher.iter(|| {
        black_box(gen_single_packet_batches(&from_keypair, &to_keypair, 8192));
    });
}

/// Measures one producer feeding a live `SigVerifyStage` worker through the
/// real sigverify channel.
fn bench_sigverify_stage_producer_consumer(bencher: &mut Bencher) {
    let (_bank, bank_forks) =
        Bank::new_with_bank_forks_for_tests(&create_genesis_config(1).genesis_config);
    let sharable_banks = bank_forks.read().unwrap().sharable_banks();
    let (packet_s, packet_r) = bounded(1024);
    let (vote_packet_s, vote_packet_r) = bounded(1024);
    let (verified_s, verified_r) = BankingTracer::channel_for_test();
    let (tpu_vote_s, _tpu_vote_r) = BankingTracer::channel_for_test();
    let (forward_stage_s, _forward_stage_r) = bounded(1024);
    let (stage, gossip_sigverify_handle) = SigVerifyStage::new(
        packet_r,
        vote_packet_r,
        verified_s,
        tpu_vote_s,
        forward_stage_s,
        // Keep this to one producer and one worker.
        NonZeroUsize::new(1).unwrap(),
        false,
        sharable_banks,
        None,
    );

    const PACKETS_PER_ITER: usize = 8192;
    let from_keypair = Keypair::new();
    let to_keypair = Keypair::new();

    bencher.iter(|| {
        let batches = gen_single_packet_batches(&from_keypair, &to_keypair, PACKETS_PER_ITER);

        let producer_packet_s = packet_s.clone();
        let producer = std::thread::spawn(move || {
            for batch in batches {
                // Let channel backpressure throttle the producer.
                producer_packet_s.send(batch).unwrap();
            }
        });

        let mut verified = 0;
        while verified < PACKETS_PER_ITER {
            // Deduper false positives can keep this below PACKETS_PER_ITER.
            match verified_r.recv_timeout(Duration::from_millis(500)) {
                Ok(batches) => {
                    verified += batches
                        .iter()
                        .map(|batch| {
                            batch
                                .iter()
                                .filter(|packet| !packet.meta().discard())
                                .count()
                        })
                        .sum::<usize>();
                }
                Err(_) if producer.is_finished() => break,
                Err(_) => {}
            }
        }
        producer.join().unwrap();
    });

    drop(packet_s);
    drop(vote_packet_s);
    drop(gossip_sigverify_handle);
    stage.join().unwrap();
}

fn prepare_batches(discard_factor: i32) -> (Vec<PacketBatch>, usize) {
    let len = 10_000; // max batch size
    let chunk_size = 1024;

    let from_keypair = Keypair::new();
    let to_keypair = Keypair::new();

    let txs: Vec<_> = (0..len)
        .map(|_| {
            let amount = rng().random();
            system_transaction::transfer(
                &from_keypair,
                &to_keypair.pubkey(),
                amount,
                Hash::default(),
            )
        })
        .collect();
    let mut batches = to_packet_batches(&txs, chunk_size);

    let mut rng = rand::rng();
    let die = Uniform::<i32>::try_from(1..100).unwrap();

    let mut c = 0;
    batches.iter_mut().for_each(|batch| {
        batch.iter_mut().for_each(|mut p| {
            let throw = die.sample(&mut rng);
            if throw < discard_factor {
                p.meta_mut().set_discard(true);
                c += 1;
            }
        })
    });
    (batches, len - c)
}

fn bench_shrink_sigverify_stage_core(bencher: &mut Bencher, discard_factor: i32) {
    let (batches0, num_valid_packets) = prepare_batches(discard_factor);
    let threadpool = Arc::new(sigverify::threadpool_for_benches());

    let mut c = 0;
    let mut total_verify_time = 0;

    bencher.iter(|| {
        let mut batches = batches0.clone();

        let mut verify_time = Measure::start("sigverify_batch_time");
        sigverify::ed25519_verify(&threadpool, &mut batches, false, num_valid_packets, false);
        verify_time.stop();
        black_box(sigverify::count_valid_packets(&batches));

        c += 1;
        total_verify_time += verify_time.as_us();
    });

    error!(
        "bsv, {}, {}",
        discard_factor,
        (total_verify_time as f64) / (c as f64),
    );
}

/// Benchmark cases for the [`bench_shrink_sigverify_stage_core`] where values represent discard factor.
const BENCH_CASES_SHRINK_SIGVERIFY_STAGE_CORE: &[i32] = &[0, 10, 20, 30, 40, 50, 60, 70, 80, 90];

fn benches() -> Vec<TestDescAndFn> {
    let mut benches = vec![
        TestDescAndFn {
            desc: TestDesc {
                name: Cow::from("bench_sigverify_stage_with_same_tx"),
                ignore: false,
            },
            testfn: TestFn::StaticBenchFn(bench_sigverify_stage_with_same_tx),
        },
        TestDescAndFn {
            desc: TestDesc {
                name: Cow::from("bench_sigverify_stage_without_same_tx"),
                ignore: false,
            },
            testfn: TestFn::StaticBenchFn(bench_sigverify_stage_without_same_tx),
        },
        TestDescAndFn {
            desc: TestDesc {
                name: Cow::from("bench_sigverify_stage_producer_consumer"),
                ignore: false,
            },
            testfn: TestFn::StaticBenchFn(bench_sigverify_stage_producer_consumer),
        },
        TestDescAndFn {
            desc: TestDesc {
                name: Cow::from("bench_gen_single_packet_batches"),
                ignore: false,
            },
            testfn: TestFn::StaticBenchFn(bench_gen_single_packet_batches),
        },
    ];

    BENCH_CASES_SHRINK_SIGVERIFY_STAGE_CORE
        .iter()
        .enumerate()
        .for_each(|(i, &discard_factor)| {
            let name = format!(
                "{i:?}-bench_shrink_sigverify_stage_core - discard_factor: {discard_factor:?}"
            );

            benches.push(TestDescAndFn {
                desc: TestDesc {
                    name: Cow::from(name),
                    ignore: false,
                },
                testfn: TestFn::DynBenchFn(Box::new(Bench(move |b: &mut Bencher| {
                    bench_shrink_sigverify_stage_core(b, discard_factor);
                }))),
            });
        });

    benches
}

benchmark_main!(benches);
