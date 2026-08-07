/*
    To run this benchmark:
    `cargo bench --bench bls_vote_sigverify`
*/

use {
    agave_bls_sigverify::unverified_vote_group::UnverifiedVoteGroup,
    agave_votor_messages::{
        consensus_message::Block,
        vote::Vote,
        wire::{VotePayloadToSign, get_vote_payload_to_sign},
    },
    criterion::{BatchSize, Criterion, criterion_group, criterion_main},
    rayon::{ThreadPool, ThreadPoolBuilder},
    solana_bls_signatures::{Keypair as BLSKeypair, PreparedHashedMessage, VerifySignature},
    solana_genesis_config::GenesisConfig,
    solana_hash::Hash,
    solana_runtime::bank::{Bank, SlotLeader},
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
) -> (VotePayloadToSign, UnverifiedVoteGroup) {
    // Pre-calculate the payloads to ensure exact distinctness
    let slot = 100;
    let vote = Vote::new_notarization_vote(Block {
        slot,
        block_id: Hash::new_unique(),
    });
    let payload = get_vote_payload_to_sign(vote, shred_version);
    let vote_payload_to_sign = VotePayloadToSign::new_from_vote(vote, shred_version);
    let mut group = UnverifiedVoteGroup::default();
    for _ in 0..batch_size {
        let bls_keypair = BLSKeypair::new();
        let signature = bls_keypair.sign(&payload);
        group.push(signature.into(), 0);
    }
    (vote_payload_to_sign, group)
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

    let leader = SlotLeader::new_unique();
    let genesis_config = GenesisConfig::default();
    let bank = Bank::new_with_paths_for_tests(&genesis_config, None, vec![], Some(leader));
    assert_eq!(*bank.leader(), leader);
    let (bank, _bank_forks) = bank.wrap_with_bank_forks_for_tests();

    for &batch_size in BATCH_SIZES {
        // Distinctness doesn't affect the cost of N individual verifications.
        let (vote_payload_to_sign, unverified_group) =
            generate_test_data(shred_version, batch_size);
        let label = format!("batch_{batch_size}");

        group.bench_function(&label, |b| {
            b.iter_batched(
                || {
                    let rank_map = bank
                        .epoch_stakes_from_slot(vote_payload_to_sign.slot())
                        .unwrap()
                        .bls_pubkey_to_rank_map();
                    let serialized_vote = wincode::serialize(&vote_payload_to_sign).unwrap();
                    let prepared_hash_msg = PreparedHashedMessage::new(&serialized_vote);
                    (rank_map, prepared_hash_msg)
                },
                |(rank_map, prepared_hash_map)| {
                    let res = unverified_group.verify_individual_votes(
                        rank_map,
                        black_box(vote_payload_to_sign),
                        black_box(prepared_hash_map),
                        &thread_pool,
                    );
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
