//! Shared test helpers for Agave crates.
//!
//! The whole crate is gated behind the `dev-context-only-utils` feature; it
//! is intended to be used as a dev-dependency only.
#![cfg(feature = "dev-context-only-utils")]

use {
    crossbeam_channel::bounded,
    solana_entry::entry::Entry,
    solana_ledger::{
        blockstore::{Blockstore, entries_to_test_shreds},
        blockstore_processor::process_entries_for_tests,
    },
    solana_rpc::transaction_status_service::TransactionStatusService,
    solana_runtime::{
        bank::Bank,
        installed_scheduler_pool::{BankWithScheduler, InstalledSchedulerPool, SchedulingContext},
        transaction_execution::TransactionStatusSender,
    },
    solana_unified_scheduler_pool::DefaultSchedulerPool,
    std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64},
    },
};

pub fn populate_blockstore_for_tests(
    entries: Vec<Entry>,
    bank: Arc<Bank>,
    blockstore: Arc<Blockstore>,
    max_complete_transaction_status_slot: Arc<AtomicU64>,
) {
    let slot = bank.slot();
    let parent_slot = bank.parent_slot();
    let shreds = entries_to_test_shreds(&entries, slot, parent_slot, true, 0);
    blockstore.insert_shreds(shreds, false).unwrap();
    blockstore.set_roots(std::iter::once(&slot)).unwrap();

    let (transaction_status_sender, transaction_status_receiver) = bounded(1024);
    let tss_exit = Arc::new(AtomicBool::new(false));
    let transaction_status_service = TransactionStatusService::new(
        transaction_status_receiver,
        max_complete_transaction_status_slot,
        true,
        None,
        blockstore,
        false,
        None,
        tss_exit.clone(),
    );

    let transaction_status_sender = TransactionStatusSender {
        sender: transaction_status_sender,
        dependency_tracker: None,
    };
    let pool = DefaultSchedulerPool::new_for_verification(
        None,
        None,
        Some(transaction_status_sender),
        None,
        None,
    );

    let context = SchedulingContext::new(bank.clone());
    let scheduler = pool.take_scheduler(context).unwrap();
    let bank = BankWithScheduler::new(bank, Some(scheduler));

    // Check that process_entries successfully writes can_commit transactions statuses, and
    // that they are matched properly by get_rooted_block
    assert_eq!(process_entries_for_tests(&bank, entries), Ok(()));

    transaction_status_service.quiesce_and_join_for_tests(tss_exit);
}
