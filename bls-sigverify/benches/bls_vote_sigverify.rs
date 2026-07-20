/*
    To run this benchmark:
    `cargo bench --bench bls_vote_sigverify`
*/

use {
    agave_bls_sigverify::bls_vote_sigverify::{UnverifiedVotePayload, verify_individual_votes},
    agave_votor_messages::{
        consensus_message::Block,
        unverified_vote_message::UnverifiedVoteMessage,
        vote::Vote,
        wire::{VotePayloadToSign, get_vote_payload_to_sign},
    },
    criterion::{BatchSize, Criterion, criterion_group, criterion_main},
    rayon::{ThreadPool, ThreadPoolBuilder},
    solana_bls_signatures::{Keypair as BLSKeypair, PreparedHashedMessage, VerifySignature},
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_signer::Signer,
    std::{hint::black_box, num::NonZero},
};

static BATCH_SIZES: &[usize] = &[8, 16, 32, 64, 128];

fn get_thread_pool() -> ThreadPool {
    let num_threads = 4;
    ThreadPoolBuilder::new()
        .num_threads(num_threads)
        .build()
        .unwrap()
}

fn generate_test_data(
    shred_version: u16,
    batch_size: usize,
) -> (VotePayloadToSign, Vec<UnverifiedVotePayload>) {
    // Pre-calculate the payloads to ensure exact distinctness
    let slot = 100;
    let vote = Vote::new_notarization_vote(Block {
        slot,
        block_id: Hash::new_unique(),
    });
    let payload = get_vote_payload_to_sign(vote, shred_version);
    (
        VotePayloadToSign::new_from_vote(vote, shred_version),
        (0..batch_size)
            .map(|_| {
                let bls_keypair = BLSKeypair::new();
                let signature = bls_keypair.sign(&payload);
                let vote_message = UnverifiedVoteMessage {
                    vote,
                    signature: signature.into(),
                    rank: 0,
                    shred_version,
                };
                UnverifiedVotePayload {
                    vote_message,
                    sender_bls_pubkey: bls_keypair.public,
                    sender_vote_account_pubkey: Keypair::new().pubkey(),
                    sender_identity_pubkey: Keypair::new().pubkey(),
                    stake: NonZero::new(123).unwrap(),
                }
            })
            .collect(),
    )
}

// Single Signature Verification
// This is just for reference
fn bench_verify_single_signature(c: &mut Criterion) {
    let mut group = c.benchmark_group("verify_single_signature");

    let keypair = BLSKeypair::new();
    let msg = b"benchmark_message_payload";
    let sig = keypair.sign(msg);
    let pubkey = keypair.public;

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

fn bench_verify_single_signature_with_prepared_message(c: &mut Criterion) {
    let mut group = c.benchmark_group("verify_single_signature_with_prepared_message");

    let keypair = BLSKeypair::new();
    let msg = b"benchmark_message_payload";
    let sig = keypair.sign(msg);
    let pubkey = keypair.public;
    let prepared_msg = PreparedHashedMessage::new(msg);

    group.bench_function("1_item", |b| {
        b.iter(|| {
            let res = pubkey.verify_signature_prepared(black_box(&sig), black_box(&prepared_msg));
            black_box(res).unwrap();
        })
    });
    group.finish();
}

// Individual Verification - verifies each signatures in parallel threads
// Message distinctness is irrelevant.
fn bench_verify_individual_votes(c: &mut Criterion) {
    let shred_version = 134;
    let mut group = c.benchmark_group("verify_votes_fallback");
    let thread_pool = get_thread_pool();

    for &batch_size in BATCH_SIZES {
        // Distinctness doesn't affect the cost of N individual verifications.
        let (_vote, unverified_votes) = generate_test_data(shred_version, batch_size);
        let label = format!("batch_{batch_size}");

        group.bench_function(&label, |b| {
            b.iter_batched(
                || unverified_votes.clone(),
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
    bench_verify_single_signature_with_prepared_message,
    bench_verify_individual_votes
);
criterion_main!(benches);
