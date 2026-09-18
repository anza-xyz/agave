/*
    To run this benchmark:
    `cargo bench --bench bls_vote_sigverify`
*/

use {
    criterion::{Criterion, criterion_group, criterion_main},
    solana_bls_signatures::{Keypair as BLSKeypair, PreparedHashedMessage, VerifySignature},
    std::hint::black_box,
};

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

criterion_group!(
    benches,
    bench_verify_single_signature,
    bench_verify_single_signature_with_prepared_message,
);
criterion_main!(benches);
