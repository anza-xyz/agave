use {
    agave_feature_set::FeatureSet,
    criterion::{Criterion, Throughput, criterion_group, criterion_main},
    solana_cost_model::cost_model::CostModel,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_message::Message,
    solana_pubkey::Pubkey,
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_signer::Signer,
    solana_system_interface::instruction as system_instruction,
    solana_transaction::{Transaction, sanitized::SanitizedTransaction},
    std::hint::black_box,
};

struct BenchSetup {
    transactions: Vec<RuntimeTransaction<SanitizedTransaction>>,
    feature_set: FeatureSet,
}

const NUM_TRANSACTIONS_PER_ITER: usize = 1024;

fn setup(num_transactions: usize) -> BenchSetup {
    let transactions = (0..num_transactions)
        .map(|_| {
            // As many transfer instructions as is possible in a regular packet.
            let from_keypair = Keypair::new();
            let to_lamports =
                Vec::from_iter(std::iter::repeat_with(|| (Pubkey::new_unique(), 1)).take(24));
            let ixs = system_instruction::transfer_many(&from_keypair.pubkey(), &to_lamports);
            let message = Message::new(&ixs, Some(&from_keypair.pubkey()));
            let transaction = Transaction::new(&[from_keypair], message, Hash::default());
            RuntimeTransaction::from_transaction_for_tests(transaction)
        })
        .collect();

    let feature_set = FeatureSet::default();

    BenchSetup {
        transactions,
        feature_set,
    }
}

fn bench_cost_model(c: &mut Criterion) {
    let BenchSetup {
        transactions,
        feature_set,
    } = setup(NUM_TRANSACTIONS_PER_ITER);

    c.benchmark_group("bench_cost_model")
        .throughput(Throughput::Elements(NUM_TRANSACTIONS_PER_ITER as u64))
        .bench_function("calculate_cost", |bencher| {
            bencher.iter(|| {
                for transaction in &transactions {
                    let _ = CostModel::calculate_cost(black_box(transaction), &feature_set);
                }
            });
        });
}

criterion_group!(benches, bench_cost_model);
criterion_main!(benches);
