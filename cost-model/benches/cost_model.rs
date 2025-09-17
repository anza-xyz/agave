#![feature(test)]
extern crate test;
use {
    agave_feature_set::FeatureSet,
    solana_cost_model::cost_model::CostModel,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_message::Message,
    solana_pubkey::Pubkey,
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_signer::Signer,
    solana_system_interface::instruction as system_instruction,
    solana_transaction::{sanitized::SanitizedTransaction, Transaction},
    test::Bencher,
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

#[bench]
fn bench_cost_model(bencher: &mut Bencher) {
    let BenchSetup {
        transactions,
        feature_set,
    } = setup(NUM_TRANSACTIONS_PER_ITER);

    bencher.iter(|| {
        for transaction in &transactions {
            let _ = CostModel::calculate_cost(test::black_box(transaction), &feature_set);
        }
    });
}
