/*
    To run this benchmark:
    `cargo bench --bench bls_vote_sigverify`
*/

use {
    agave_votor_messages::{consensus_message::VoteMessage, vote::Vote},
    criterion::{BatchSize, Criterion, criterion_group, criterion_main},
    rayon::{ThreadPool, ThreadPoolBuilder},
    solana_bls_signatures::{Keypair as BLSKeypair, Pubkey as BLSPubkey, VerifiablePubkey},
    solana_core::bls_sigverify::{
        bls_vote_sigverify::{
            VotePayload, aggregate_pubkeys_by_payload, aggregate_signatures,
            verify_individual_votes, verify_votes_optimistic,
        },
        stats::SigVerifyVoteStats,
    },
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_signer::Signer,
    std::{hint::black_box, sync::Arc},
};

static MESSAGE_COUNTS: &[usize] = &[1, 2, 4, 8, 16];
static BATCH_SIZES: &[usize] = &[8, 16, 32, 64, 128];

fn get_thread_pool() -> ThreadPool {
    let num_threads = 4;
    ThreadPoolBuilder::new()
        .num_threads(num_threads)
        .build()
        .unwrap()
}

fn get_matrix_params() -> impl Iterator<Item = (usize, usize)> {
    BATCH_SIZES.iter().flat_map(|&batch_size| {
        MESSAGE_COUNTS.iter().filter_map(move |&num_distinct| {
            if num_distinct > batch_size {
                None
            } else {
                Some((batch_size, num_distinct))
            }
        })
    })
}

fn generate_test_data(num_distinct_messages: usize, batch_size: usize) -> Vec<VotePayload> {
    assert!(
        batch_size >= num_distinct_messages,
        "Batch size must be >= distinct messages"
    );

    // Pre-calculate the payloads to ensure exact distinctness
    let base_payloads: Vec<Arc<Vec<u8>>> = (0..num_distinct_messages)
        .map(|i| {
            let slot = (i as u64).saturating_add(100);
            let vote = Vote::new_notarization_vote(slot, Hash::new_unique());
            Arc::new(bincode::serialize(&vote).unwrap())
        })
        .collect();

    let mut votes_to_verify = Vec::with_capacity(batch_size);

    for i in 0..batch_size {
        let payload = &base_payloads[i.rem_euclid(num_distinct_messages)];

        let bls_keypair = BLSKeypair::new();
        let vote: Vote = bincode::deserialize(payload).unwrap();

        let signature = bls_keypair.sign(payload);

        let vote_message = VoteMessage {
            vote,
            signature: signature.into(),
            rank: 0,
        };

        votes_to_verify.push(VotePayload {
            vote_message,
            bls_pubkey: bls_keypair.public,
            pubkey: Keypair::new().pubkey(),
            remote_pubkey: Keypair::new().pubkey(),
        });
    }

    votes_to_verify
}

// Single Signature Verification
// This is just for reference
fn bench_verify_single_signature(c: &mut Criterion) {
    let mut group = c.benchmark_group("verify_single_signature");

    let keypair = BLSKeypair::new();
    let msg = b"benchmark_message_payload";
    let sig = keypair.sign(msg);
    let pubkey: BLSPubkey = keypair.public.into();

    group.bench_function("1_item", |b| {
        b.iter(|| {
            // We use the raw verify method from the underlying library
            // to establish the cryptographic floor.
            let res = pubkey.verify_signature(black_box(&sig), black_box(msg));
            black_box(res).unwrap();
        })
    });
    group.finish();
}

// Optimistic Verification - aggregates the public keys and signatures first before verifying.
// Depends on both batch size and message distinctness due to pairing checks.
fn bench_verify_votes_optimistic(c: &mut Criterion) {
    let mut group = c.benchmark_group("verify_votes_optimistic");
    let mut stats = SigVerifyVoteStats::default();
    let thread_pool = get_thread_pool();

    for (batch_size, num_distinct) in get_matrix_params() {
        let votes = generate_test_data(num_distinct, batch_size);
        let label = format!("msgs_{num_distinct}/batch_{batch_size}");

        group.bench_function(&label, |b| {
            b.iter(|| {
                let res = verify_votes_optimistic(black_box(&votes), &mut stats, &thread_pool);
                black_box(res);
            })
        });
    }
    group.finish();
    black_box(stats);
}

// Public Key Aggregation
// Depends on message distinctness because keys are grouped by messages.
fn bench_aggregate_pubkeys(c: &mut Criterion) {
    let mut group = c.benchmark_group("aggregate_pubkeys");
    let mut stats = SigVerifyVoteStats::default();

    for (batch_size, num_distinct) in get_matrix_params() {
        let votes = generate_test_data(num_distinct, batch_size);
        let label = format!("msgs_{num_distinct}/batch_{batch_size}");

        group.bench_function(&label, |b| {
            b.iter(|| {
                let res = aggregate_pubkeys_by_payload(black_box(&votes), &mut stats);
                black_box(res).1.unwrap();
            })
        });
    }
    group.finish();
    black_box(stats);
}

// Signature Aggregation
// Pure G1 addition - message distinctness is irrelevant.
fn bench_aggregate_signatures(c: &mut Criterion) {
    let mut group = c.benchmark_group("aggregate_signatures");

    for &batch_size in BATCH_SIZES {
        // Use 1 distinct message just to generate valid data cheaply.
        // It doesn't affect signature aggregation performance.
        let votes = generate_test_data(1, batch_size);
        let label = format!("batch_{batch_size}");

        group.bench_function(&label, |b| {
            b.iter(|| {
                let res = aggregate_signatures(black_box(&votes));
                black_box(res).unwrap();
            })
        });
    }
    group.finish();
}

// Individual Verification - verifies each signatures in parallel threads
// Message distinctness is irrelevant.
fn bench_verify_individual_votes(c: &mut Criterion) {
    let mut group = c.benchmark_group("verify_votes_fallback");
    let thread_pool = get_thread_pool();

    for &batch_size in BATCH_SIZES {
        // Distinctness doesn't affect the cost of N individual verifications.
        let votes = generate_test_data(1, batch_size);
        let label = format!("batch_{batch_size}");

        group.bench_function(&label, |b| {
            b.iter_batched(
                || votes.clone(),
                |votes| {
                    let res = verify_individual_votes(black_box(votes), &thread_pool);
                    black_box(res);
                },
                BatchSize::SmallInput,
            )
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_verify_single_signature,
    bench_verify_votes_optimistic,
    bench_aggregate_pubkeys,
    bench_aggregate_signatures,
    bench_verify_individual_votes
);
criterion_main!(benches);
