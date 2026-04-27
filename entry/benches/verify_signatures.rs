use {
    agave_reserved_account_keys::ReservedAccountKeys,
    criterion::{Criterion, Throughput, criterion_group, criterion_main},
    solana_entry::entry::{Entry, UnverifiedSignatures, validate_and_hash_transactions},
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_message::SimpleAddressLoader,
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_signer::Signer,
    solana_system_transaction::transfer,
    solana_transaction::{
        sanitized::{MessageHash, SanitizedTransaction},
        versioned::VersionedTransaction,
    },
    solana_transaction_error::TransactionResult as Result,
    std::hint::black_box,
};

fn build_unverified_signatures(num_transactions: usize) -> UnverifiedSignatures {
    let thread_pool = solana_entry::entry::thread_pool_for_benches();
    let hash = Hash::default();
    let keypair = Keypair::new();
    let transactions = (0..num_transactions)
        .map(|lamports| transfer(&keypair, &keypair.pubkey(), lamports as u64, hash))
        .collect();
    let entries = vec![Entry::new(&hash, 0, transactions)];

    let validate_transaction = move |versioned_tx: VersionedTransaction,
                                     message_bytes: &[u8]|
          -> Result<RuntimeTransaction<SanitizedTransaction>> {
        RuntimeTransaction::try_create(
            versioned_tx,
            MessageHash::Precomputed(solana_message::VersionedMessage::hash_raw_message(
                message_bytes,
            )),
            None,
            SimpleAddressLoader::Disabled,
            &ReservedAccountKeys::empty_key_set(),
            true,
        )
    };

    validate_and_hash_transactions(
        entries,
        num_transactions,
        &thread_pool,
        validate_transaction,
    )
    .expect("transaction validation should succeed")
    .unverified_signatures
}

fn bench_verify_signatures(c: &mut Criterion) {
    for num_transactions in [1, 32, 256, 1024, 4096] {
        let unverified_signatures = build_unverified_signatures(num_transactions);
        let mut group = c.benchmark_group("entry_verify_signatures");
        group.throughput(Throughput::Elements(num_transactions as u64));

        group.bench_function(format!("single_loop/{num_transactions}_txs"), |bencher| {
            bencher.iter(|| {
                black_box(&unverified_signatures)
                    .verify_single_loop_for_benches()
                    .expect("signatures should verify");
            });
        });

        group.bench_function(
            format!("extract_then_verify/{num_transactions}_txs"),
            |bencher| {
                bencher.iter(|| {
                    black_box(&unverified_signatures)
                        .verify()
                        .expect("signatures should verify");
                });
            },
        );

        group.finish();
    }
}

criterion_group!(benches, bench_verify_signatures);
criterion_main!(benches);
