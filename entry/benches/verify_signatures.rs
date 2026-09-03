use {
    agave_reserved_account_keys::ReservedAccountKeys,
    agave_transaction_view::transaction_view::{
        SanitizedTransactionView, UnsanitizedTransactionView,
    },
    bytes::Bytes,
    criterion::{Criterion, Throughput, criterion_group, criterion_main},
    solana_entry::entry::{
        Entry, UnverifiedSignatures, entry_views_for_tests, validate_and_hash_transactions,
    },
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_runtime_transaction::{
        runtime_transaction::{ReplayTransaction, RuntimeTransaction},
        sanitize_config::sanitize_config,
    },
    solana_signer::Signer,
    solana_system_transaction::transfer,
    solana_transaction::sanitized::MessageHash,
    solana_transaction_error::{TransactionError, TransactionResult as Result},
    std::hint::black_box,
};

fn build_unverified_signatures(num_transactions: usize) -> UnverifiedSignatures<Bytes> {
    let thread_pool = solana_entry::entry::thread_pool_for_benches();
    let hash = Hash::default();
    let keypair = Keypair::new();
    let transactions = (0..num_transactions)
        .map(|lamports| transfer(&keypair, &keypair.pubkey(), lamports as u64, hash))
        .collect();
    let entries = entry_views_for_tests(vec![Entry::new(&hash, 0, transactions)]);

    let validate_transaction =
        move |unsanitized: UnsanitizedTransactionView<Bytes>| -> Result<ReplayTransaction> {
            let sanitized = unsanitized
                .sanitize(&sanitize_config())
                .map_err(|_| TransactionError::SanitizeFailure)?;
            let statically_loaded = RuntimeTransaction::<SanitizedTransactionView<Bytes>>::try_new(
                sanitized,
                MessageHash::Compute,
                None,
            )?;
            ReplayTransaction::try_new(
                statically_loaded,
                None,
                &ReservedAccountKeys::empty_key_set(),
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
