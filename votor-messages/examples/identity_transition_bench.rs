//! Run with `cargo run --release -p agave-votor-messages --features agave-unstable-api
//! --example identity_transition_bench`. These are microbenchmarks, not validator profiles.
#![allow(clippy::arithmetic_side_effects)]

use {
    agave_votor_messages::identity_transition::{
        IdentityTransitionConsensus, IdentityTransitionTracker, SubmittedVoteSlots,
    },
    crossbeam_channel::bounded,
    solana_pubkey::Pubkey,
    std::{hint::black_box, time::Instant},
};

fn median_ns(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

fn submission_sample(observe: bool, iterations: u64) -> f64 {
    let (sender, receiver) = bounded(1);
    let mut slots = SubmittedVoteSlots::default();
    let start = Instant::now();
    for slot in 0..iterations {
        sender.send(black_box(slot)).unwrap();
        if observe {
            black_box(&mut slots).record(slot);
        }
        black_box(receiver.recv().unwrap());
    }
    black_box(slots.highest());
    start.elapsed().as_nanos() as f64 / iterations as f64
}

fn main() {
    let mut baseline = Vec::new();
    let mut observed = Vec::new();
    for round in 0..11 {
        if round % 2 == 0 {
            baseline.push(submission_sample(false, 1_000_000));
            observed.push(submission_sample(true, 1_000_000));
        } else {
            observed.push(submission_sample(true, 1_000_000));
            baseline.push(submission_sample(false, 1_000_000));
        }
    }
    let baseline = median_ns(baseline);
    let observed = median_ns(observed);
    println!("enqueue/dequeue baseline median: {baseline:.2} ns/op");
    println!("enqueue/dequeue + local observation median: {observed:.2} ns/op");
    println!(
        "local observation delta: {:.2} ns/op ({:.2}%)",
        observed - baseline,
        (observed / baseline - 1.0) * 100.0
    );

    let a = Pubkey::new_from_array([1; 32]);
    let b = Pubkey::new_from_array([2; 32]);
    let account = Pubkey::new_from_array([3; 32]);
    let tracker = IdentityTransitionTracker::default();
    let iterations = 100_000;
    let start = Instant::now();
    for _ in 0..iterations {
        let seq = tracker.begin(a, b, account, IdentityTransitionConsensus::Tower, true);
        tracker.finish_command(seq, None);
        tracker.acknowledge(
            Some(seq),
            a,
            b,
            IdentityTransitionConsensus::Tower,
            Some(123),
            Some(90),
        );
    }
    println!(
        "begin + command finish + adoption: {:.2} ns/transition",
        start.elapsed().as_nanos() as f64 / iterations as f64
    );
    let start = Instant::now();
    for _ in 0..iterations {
        black_box(tracker.get(b));
    }
    println!(
        "status snapshot + identity formatting: {:.2} ns/query",
        start.elapsed().as_nanos() as f64 / iterations as f64
    );
}
