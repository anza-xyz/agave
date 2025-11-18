#![feature(test)]
extern crate test;
use {
    agave_reserved_account_keys::ReservedAccountKeys,
    solana_entry::entry,
    solana_hash::Hash,
    solana_message::SimpleAddressLoader,
    solana_perf::test_tx::test_tx,
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_transaction::{
        sanitized::{MessageHash, SanitizedTransaction},
        versioned::VersionedTransaction,
    },
    solana_transaction_error::TransactionResult as Result,
    std::sync::Arc,
    test::Bencher,
};

#[bench]
fn bench_cpusigverify(bencher: &mut Bencher) {
    let thread_pool = entry::thread_pool_for_benches();
    let entries = (0..131072)
        .map(|_| {
            let transaction = test_tx();
            entry::next_entry_mut(&mut Hash::default(), 0, vec![transaction])
        })
        .collect::<Vec<_>>();

    let verify_transaction = {
        move |versioned_tx: VersionedTransaction| -> Result<RuntimeTransaction<SanitizedTransaction>> {
            let sanitized_tx = {
                let message_hash = versioned_tx.verify_and_hash_message()?;
                RuntimeTransaction::try_create(
                    versioned_tx,
                    MessageHash::Precomputed(message_hash),
                    None,
                    SimpleAddressLoader::Disabled,
                    &ReservedAccountKeys::empty_key_set(),
                    true,
                )
            }?;

            Ok(sanitized_tx)
        }
    };

    bencher.iter(|| {
        let _ans =
            entry::verify_transactions(entries.clone(), &thread_pool, Arc::new(verify_transaction));
    })
}
