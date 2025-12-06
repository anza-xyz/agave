//! This module contains any integration code to put the unified scheduler subsystem into the
//! banking stage, starting from `BankingPacketBatch` ingestion from the sig verify stage and to
//! `Task` submission to the unified scheduler.
//!
//! These preprocessing for task creation can be multi-threaded trivially, including
//! unified-scheduler specific task of UsageQueue lookups with BankingStageHelper. At the same
//! time, the maximum cpu core utilization needs to be constrained among this processing and the
//! actual task handling of unified scheduler. Thus, it's desired to share a single thread pool for
//! the two kinds of work. Towards that end, the integration was implemented as a callback-style,
//! which is invoked (via `select!` on `banking_packet_receiver`) at each of unified scheduler
//! handler threads.
//!
//! Aside from some limited abstraction leakage to make `select!` work at the
//! solana-unified-scheduler-pool crate, almost all of these preprocessing are intentionally
//! encapsulated into this module, at the cost of dynamic dispatch per each BankingPacketBatch to
//! retain the unified scheduler agnostic scheduler over scheduling mode (block verification vs
//! block production) as much as possible.
//!
//! Lastly, what the callback closure in this module does is roughly as follows:
//! 1. Translate the raw packet bytes into some structs
//! 2. Do various sanitization on them
//! 3. Calculate priorities
//! 4. Convert them to tasks with the help of provided BankingStageHelper (remember that pubkey
//!    lookup for UsageQueue is also performed at this step by UsageQueueLoaderInner
//!    internally; thus multi-threaded and off-loaded from the single-threaded-by-design
//!    scheduler thread)
//! 5. Submit the tasks.

#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    super::{
        decision_maker::{BufferedPacketsDecision, DecisionMaker, DecisionMakerWrapper},
        transaction_scheduler::receive_and_buffer::calculate_priority_and_cost,
    },
    crate::banking_trace::Channels,
    agave_banking_stage_ingress_types::BankingPacketBatch,
    solana_accounts_db::account_locks::validate_account_locks,
    solana_address_lookup_table_interface::state::estimate_last_valid_slot,
    solana_clock::Slot,
    solana_message::{v0::LoadedAddresses, SimpleAddressLoader},
    solana_poh::{poh_recorder::PohRecorder, transaction_recorder::TransactionRecorder},
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    solana_runtime_transaction::{
        runtime_transaction::RuntimeTransaction, transaction_meta::StaticMeta,
    },
    solana_svm_transaction::{
        message_address_table_lookup::SVMMessageAddressTableLookup, svm_message::SVMMessage,
    },
    solana_transaction::{
        sanitized::{MessageHash, SanitizedTransaction},
        versioned::{sanitized::SanitizedVersionedTransaction, VersionedTransaction},
    },
    solana_transaction_error::AddressLoaderError,
    solana_unified_scheduler_pool::{BankingStageHelper, DefaultSchedulerPool},
    std::{
        num::NonZeroUsize,
        ops::Deref,
        sync::{Arc, RwLock},
    },
};

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) fn ensure_banking_stage_setup(
    pool: &DefaultSchedulerPool,
    bank_forks: &Arc<RwLock<BankForks>>,
    channels: &Channels,
    poh_recorder: &Arc<RwLock<PohRecorder>>,
    transaction_recorder: TransactionRecorder,
    num_threads: NonZeroUsize,
) {
    if !pool.block_production_supported() {
        return;
    }

    let sharable_banks = bank_forks.read().unwrap().sharable_banks();
    let banking_packet_receiver = channels.receiver_for_unified_scheduler().clone();

    let (is_exited, decision_maker) = {
        let poh_recorder = poh_recorder.read().unwrap();
        (
            poh_recorder.is_exited.clone(),
            DecisionMaker::from(poh_recorder.deref()),
        )
    };

    let banking_stage_monitor = Box::new(DecisionMakerWrapper::new(
        channels.clone_is_unified_for_unified_scheduler(),
        is_exited,
        decision_maker.clone(),
    ));
    let banking_packet_handler = Box::new(
        move |helper: &BankingStageHelper, batches: BankingPacketBatch| {
            let decision = decision_maker.make_consume_or_forward_decision();
            if matches!(decision, BufferedPacketsDecision::Forward) {
                // discard newly-arriving packets. note that already handled packets (thus buffered
                // by scheduler internally) will be discarded as well via BankingStageMonitor api
                // by solScCleaner.
                return;
            }
            let bank = sharable_banks.root();
            for batch in batches.iter() {
                // over-provision nevertheless some of packets could be invalid.
                let task_id_base = helper.generate_task_ids(batch.len());
                let tasks = batch.iter().enumerate().filter_map(|(i, packet)| {
                    // Deserialize & sanitize.
                    let tx: VersionedTransaction = packet.deserialize_slice(..).ok()?;
                    let tx = SanitizedVersionedTransaction::try_from(tx).ok()?;

                    // Resolve to runtime transaction.
                    let tx = RuntimeTransaction::<SanitizedVersionedTransaction>::try_from(
                        tx,
                        MessageHash::Compute,
                        Some(packet.meta().is_simple_vote_tx()),
                    )
                    .ok()?;

                    let (loaded_addresses, deactivation_slot) =
                        resolve_addresses_with_deactivation(&tx, &bank).ok()?;
                    let tx = RuntimeTransaction::<SanitizedTransaction>::try_from(
                        tx,
                        SimpleAddressLoader::Enabled(loaded_addresses),
                        bank.get_reserved_account_keys(),
                    )
                    .ok()?;
                    validate_account_locks(
                        tx.account_keys(),
                        bank.get_transaction_account_lock_limit(),
                    )
                    .ok()?;

                    // Determine priority.
                    let compute_budget_limits = tx
                        .compute_budget_instruction_details()
                        .sanitize_and_convert_to_compute_budget_limits(&bank.feature_set)
                        .ok()?;
                    let (priority, _cost) =
                        calculate_priority_and_cost(&tx, &compute_budget_limits.into(), &bank);
                    let task_id = BankingStageHelper::new_task_id(task_id_base + i, priority);

                    Some(helper.create_new_task(
                        tx,
                        task_id,
                        packet.meta().size,
                        bank.epoch(),
                        estimate_last_valid_slot(bank.slot().min(deactivation_slot)),
                    ))
                });

                for task in tasks {
                    helper.send_new_task(task);
                }
            }
        },
    );

    pool.register_banking_stage(
        Some(num_threads.get()),
        banking_packet_receiver,
        banking_packet_handler,
        transaction_recorder,
        banking_stage_monitor,
    );
}

fn resolve_addresses_with_deactivation(
    transaction: &SanitizedVersionedTransaction,
    bank: &Bank,
) -> Result<(LoadedAddresses, Slot), AddressLoaderError> {
    let Some(address_table_lookups) = transaction.get_message().message.address_table_lookups()
    else {
        return Ok((LoadedAddresses::default(), Slot::MAX));
    };

    bank.load_addresses_from_ref(
        address_table_lookups
            .iter()
            .map(SVMMessageAddressTableLookup::from),
    )
}
