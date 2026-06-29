/*
    To run this benchmark:
    `cargo bench --bench bls_vote_sigverify`
*/

use {
    agave_bls_sigverify::{
        bls_vote_sigverify::{
            UnverifiedVotePayload, aggregate_pubkeys_by_payload, aggregate_signatures,
            verify_individual_votes, verify_votes_optimistic,
        },
        stats::SigVerifyVoteStats,
    },
    agave_votor_messages::{
        consensus_message::Block,
        unverified_vote_message::UnverifiedVoteMessage,
        vote::Vote,
        wire::{VotePayloadToSign, get_vote_payload_to_sign},
    },
    criterion::{BatchSize, Criterion, criterion_group, criterion_main},
    rayon::{ThreadPool, ThreadPoolBuilder},
    solana_bls_signatures::{Keypair as BLSKeypair, PreparedHashedMessage, VerifySignature},
    solana_genesis_config::GenesisConfig,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_runtime::bank::{Bank, SlotLeader},
    solana_signer::Signer,
    std::hint::black_box,
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
                    prepared_payload: None,
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

// Optimistic Verification - aggregates the public keys and signatures first before verifying.
// Depends on both batch size and message distinctness due to pairing checks.
fn bench_verify_votes_optimistic(c: &mut Criterion) {
    let shred_version = 1234;
    let mut group = c.benchmark_group("verify_votes_optimistic");
    let mut stats = SigVerifyVoteStats::default();
    let thread_pool = get_thread_pool();

    for &batch_size in BATCH_SIZES {
        let (vote, mut unverified_votes) = generate_test_data(shred_version, batch_size);
        let label = format!("batch_{batch_size}");

        group.bench_function(&label, |b| {
            b.iter(|| {
                let res = verify_votes_optimistic(
                    vote,
                    black_box(&mut unverified_votes),
                    &mut stats,
                    &thread_pool,
                );
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
    let shred_version = 1234;
    let mut group = c.benchmark_group("aggregate_pubkeys");

    for &batch_size in BATCH_SIZES {
        let (vote, unverified_votes) = generate_test_data(shred_version, batch_size);
        let label = format!("batch_{batch_size}");

        group.bench_function(&label, |b| {
            b.iter(|| {
                let res = aggregate_pubkeys_by_payload(vote, black_box(&unverified_votes));
                black_box(res).1.unwrap();
            })
        });
    }
    group.finish();
}

// Signature Aggregation
// Pure G1 addition - message distinctness is irrelevant.
fn bench_aggregate_signatures(c: &mut Criterion) {
    let shred_version = 1234;
    let mut group = c.benchmark_group("aggregate_signatures");

    for &batch_size in BATCH_SIZES {
        // Use 1 distinct message just to generate valid data cheaply.
        // It doesn't affect signature aggregation performance.
        let (_, unverified_votes) = generate_test_data(shred_version, batch_size);
        let label = format!("batch_{batch_size}");

        group.bench_function(&label, |b| {
            b.iter(|| {
                let res = aggregate_signatures(black_box(&unverified_votes));
                black_box(res).unwrap();
            })
        });
    }
    group.finish();
}

// Individual Verification - verifies each signatures in parallel threads
// Message distinctness is irrelevant.
fn bench_verify_individual_votes(c: &mut Criterion) {
    let shred_version = 134;
    let mut group = c.benchmark_group("verify_votes_fallback");
    let thread_pool = get_thread_pool();

    let leader = SlotLeader::new_unique();
    let genesis_config = GenesisConfig::default();
    let bank = Bank::new_with_paths_for_tests(&genesis_config, None, vec![], Some(leader));
    assert_eq!(*bank.leader(), leader);
    let (bank, _bank_forks) = bank.wrap_with_bank_forks_for_tests();

    for &batch_size in BATCH_SIZES {
        // Distinctness doesn't affect the cost of N individual verifications.
        let (_vote, unverified_votes) = generate_test_data(shred_version, batch_size);
        let label = format!("batch_{batch_size}");

        group.bench_function(&label, |b| {
            b.iter_batched(
                || unverified_votes.clone(),
                |votes| {
                    let res = verify_individual_votes(&bank, black_box(votes), &thread_pool);
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
    bench_verify_votes_optimistic,
    bench_aggregate_pubkeys,
    bench_aggregate_signatures,
    bench_verify_individual_votes
);
criterion_main!(benches);
