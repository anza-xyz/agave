//! Bank cleanup for Alpenglow block switches and UpdateParent restarts.
//!
//! Tower duplicate repair and the migration purge use `purge_unconfirmed_slot`
//! in the parent module.

use {
    super::{ReplayStage, ReplayStageError},
    crate::consensus::progress_map::ProgressMap,
    solana_clock::Slot,
    solana_ledger::blockstore_processor::AsyncVerificationProgress,
    solana_runtime::{
        bank_forks::BankForks,
        transaction_execution::{
            TransactionHistoryPurgeInput, TransactionHistoryPurgeSource, TransactionStatusSender,
        },
    },
    std::{collections::BTreeSet, sync::RwLock},
};

/// Clear the requested slots and their descendants from progress, bank forks, and shared
/// caches. Requested slots are purged from shared caches even if their banks no longer exist.
pub(super) fn clear_slots(
    slots_to_clear: impl IntoIterator<Item = Slot>,
    bank_forks: &RwLock<BankForks>,
    progress: &mut ProgressMap,
    async_verification_freelist: &mut Vec<AsyncVerificationProgress>,
    transaction_history_purge: Option<(
        &TransactionStatusSender,
        TransactionHistoryPurgeSource,
        TransactionHistoryPurgeInput,
    )>,
) -> Result<(), ReplayStageError> {
    let (slots_to_purge, banks_to_clear) =
        bank_forks.read().unwrap().slots_to_clear(slots_to_clear);
    if slots_to_purge.is_empty() {
        return Ok(());
    }

    // Wait for async verify to complete
    for slot in &slots_to_purge {
        if let Some(replay_progress) = progress.remove(slot) {
            let mut w_replay_progress = replay_progress.replay_progress.write().unwrap();
            let _ = w_replay_progress.wait_for_all_verification_results(&mut 0, &mut 0);
            ReplayStage::recycle_async_verification(
                async_verification_freelist,
                w_replay_progress.take_async_verification(),
            );
        }
    }

    // Wait for any in progress execution
    for bank in banks_to_clear.iter() {
        bank.quiesce_transaction_execution();
        let _ = bank.wait_for_completed_scheduler();
    }
    let bank_slots_to_clear = banks_to_clear
        .iter()
        .map(|bank| bank.slot())
        .collect::<BTreeSet<_>>();

    // Only dump banks that are still present. `slots_to_purge` can also contain slots that
    // were already removed, but whose shared cache entries still need to be cleared before the
    // slots are revived.
    let (root_bank, slot_bank_ids_to_purge, removed_banks) = {
        let mut w_bank_forks = bank_forks.write().unwrap();

        let root_bank = w_bank_forks.root_bank();
        let bank_slots_to_clear = bank_slots_to_clear
            .into_iter()
            .filter(|slot| w_bank_forks.get(*slot).is_some())
            .collect::<BTreeSet<_>>();
        let (slot_bank_ids_to_purge, removed_banks) =
            w_bank_forks.dump_slots(bank_slots_to_clear.iter(), false);
        (root_bank, slot_bank_ids_to_purge, removed_banks)
    };

    // Mark these banks removed so that any ongoing RPC scans fail.
    // These have to be atomically marked together in the same batch, in order
    // to prevent RPC from seeing inconsistent results in scans.
    if !slot_bank_ids_to_purge.is_empty() {
        root_bank.mark_banks_removed(&slot_bank_ids_to_purge);
    }

    // Once the banks above are marked, it's safe to drop them. Bank::drop() purges and
    // unmarks them, so it must not run before the `mark_banks_removed()` call.
    drop(banks_to_clear);
    drop(removed_banks);

    // Clear the shared caches even for requested slots whose banks were already removed.
    // Those slots can be revived by an Alpenglow switch, and stale entries from the old block
    // version must not affect replay of the new version.
    for slot in &slots_to_purge {
        root_bank.clear_slot_signatures(*slot);
        root_bank.prune_program_cache_by_deployment_slot(*slot);
    }

    if let Some((transaction_status_sender, source, input)) = transaction_history_purge {
        for slot in slots_to_purge {
            match &input {
                TransactionHistoryPurgeInput::ReplayStage => {
                    // TSS processes the discarded prefix writes, this purge, and restarted
                    // execution writes in channel order, so replay does not need to wait.
                    transaction_status_sender
                        .send_purge_transaction_history_for_slot(
                            slot,
                            source,
                            TransactionHistoryPurgeInput::ReplayStage,
                            /*wait_until_finished:*/ false,
                        )
                        .map_err(|source| ReplayStageError::PurgeTransactionHistory {
                            slot,
                            source,
                        })?;
                }
                TransactionHistoryPurgeInput::SwitchBank => {
                    // Wait while the outgoing slot's shreds are still present; the purge must
                    // reconstruct their deletion keys before SwitchBank replaces those shreds.
                    transaction_status_sender
                        .send_purge_transaction_history_for_slot(
                            slot,
                            source,
                            TransactionHistoryPurgeInput::SwitchBank,
                            /*wait_until_finished:*/ true,
                        )
                        .map_err(|source| ReplayStageError::PurgeTransactionHistory {
                            slot,
                            source,
                        })?;
                }
                TransactionHistoryPurgeInput::Leader(_) => {
                    unreachable!("leader transaction-history purge cannot clear replay slots")
                }
            }
        }
    }
    Ok(())
}
