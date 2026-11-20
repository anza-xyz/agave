use {
    agave_transaction_view::transaction_view::SanitizedTransactionView,
    criterion::{BenchmarkId, Criterion, criterion_group, criterion_main},
    solana_hash::Hash,
    solana_instruction::Instruction,
    solana_keypair::Keypair,
    solana_message::{VersionedMessage, legacy},
    solana_pubkey::Pubkey,
    solana_runtime_transaction::{
        leader_transaction_notifier::{LeaderTransactionBatch, LeaderTransactionNotifier},
        runtime_transaction::RuntimeTransaction,
        transaction_with_meta::StaticTransactionWithMeta,
    },
    solana_signer::Signer,
    solana_transaction::{sanitized::MessageHash, versioned::VersionedTransaction},
    std::{hint::black_box, time::Duration},
};

fn benchmark(c: &mut Criterion) {
    let payer = Keypair::new();
    let mut group = c.benchmark_group("leader_notification_preparation");
    group
        .sample_size(30)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(2));
    for instructions in [1, 12] {
        let instructions: Vec<_> = (0..instructions)
            .map(|_| Instruction::new_with_bytes(Pubkey::new_unique(), &[7; 24], vec![]))
            .collect();
        let tx = VersionedTransaction::try_new(
            VersionedMessage::Legacy(legacy::Message::new_with_blockhash(
                &instructions,
                Some(&payer.pubkey()),
                &Hash::default(),
            )),
            &[&payer],
        )
        .unwrap();
        let bytes = wincode::serialize(&tx).unwrap();
        let view = RuntimeTransaction::<SanitizedTransactionView<_>>::try_new(
            SanitizedTransactionView::try_new_sanitized(bytes.as_slice()).unwrap(),
            MessageHash::Compute,
            None,
        )
        .unwrap();
        for count in [1, 64] {
            let shape = format!("{}ix/{count}tx", instructions.len());
            group.bench_with_input(
                BenchmarkId::new("reconstruct", &shape),
                &count,
                |b, &count| {
                    b.iter(|| {
                        let txs: Vec<_> = (0..count)
                            .map(|_| black_box(&view).to_versioned_transaction())
                            .collect();
                        black_box(txs);
                    })
                },
            );
            group.bench_with_input(
                BenchmarkId::new("copy_batch", &shape),
                &count,
                |b, &count| {
                    b.iter(|| {
                        let mut batch = LeaderTransactionBatch::new(
                            42,
                            7,
                            count,
                            bytes.len().checked_mul(count).unwrap(),
                        );
                        for _ in 0..count {
                            batch.push(black_box(&view), Ok(()));
                        }
                        black_box(batch);
                    })
                },
            );
            let (notifier, receiver) = LeaderTransactionNotifier::channel(1);
            group.bench_with_input(
                BenchmarkId::new("copy_enqueue_drain", &shape),
                &count,
                |b, &count| {
                    b.iter(|| {
                        let mut batch = LeaderTransactionBatch::new(
                            42,
                            7,
                            count,
                            bytes.len().checked_mul(count).unwrap(),
                        );
                        for _ in 0..count {
                            batch.push(black_box(&view), Ok(()));
                        }
                        notifier.notify_transactions(batch, 0);
                        black_box(receiver.try_recv().unwrap());
                    })
                },
            );
        }
    }
    group.finish();
}
criterion_group!(benches, benchmark);
criterion_main!(benches);
