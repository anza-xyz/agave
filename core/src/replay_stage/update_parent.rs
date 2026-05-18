//! Fast-leader-handover replay helpers.

use {
    super::{
        ProcessActiveBanksContext, ReplayStage, TowerBFTStructures,
        dead_slots::{
            DeadSlotLogLevel, DeadSlotNotifications, mark_hard_dead_slot,
            mark_hard_dead_slot_notifications, send_invalid_bank,
        },
    },
    crate::{
        consensus::progress_map::{DeadSlotReason, ProgressMap},
        repair::cluster_slot_state_verifier::{DuplicateSlotsToRepair, PurgeRepairSlotCounter},
    },
    agave_votor_messages::migration::MigrationStatus,
    solana_clock::Slot,
    solana_entry::block_component::VersionedUpdateParent,
    solana_ledger::{
        blockstore::{Blockstore, UpdateParentReceiver},
        blockstore_meta::SlotMeta,
        blockstore_processor::AsyncVerificationProgress,
    },
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::BankForks, vote_sender_types::ReplayVoteSender},
    std::{
        collections::BTreeSet,
        sync::{Arc, RwLock},
    },
};

/// Replay-start decision for a child bank discovered from blockstore metadata.
pub(super) enum ChildBankReplayStart {
    /// Create the bank and replay from shred zero.
    FromStart,
    /// Create the bank and begin replay at the `UpdateParent` FEC-set boundary.
    FromUpdateParent(u64),
    /// Do not create the bank yet. The local parent information is not
    /// coherent enough to safely choose a replay start.
    Defer,
}

/// Return the UpdateParent replay offset recorded in SlotMeta, if it is
/// usable from the currently rooted fork.
fn update_parent_replay_offset(slot: Slot, slot_meta: &SlotMeta, root: Slot) -> Option<u64> {
    if !slot_meta.has_update_parent() {
        return None;
    }
    let parent_slot = slot_meta.parent_slot?;
    (root <= parent_slot && parent_slot < slot).then_some(u64::from(slot_meta.replay_fec_set_index))
}

/// Decide whether a newly discovered child bank should replay from the
/// beginning, from an `UpdateParent` FEC set, or wait for coherent metadata.
pub(super) fn child_bank_replay_start(
    blockstore: &Blockstore,
    parent_bank: &Bank,
    parent_slot: Slot,
    child_slot: Slot,
    migration_status: &MigrationStatus,
) -> ChildBankReplayStart {
    if !migration_status.should_allow_block_markers(child_slot) {
        return ChildBankReplayStart::FromStart;
    }

    let Some(slot_meta) = blockstore
        .meta(child_slot)
        .expect("Blockstore should not fail")
    else {
        return ChildBankReplayStart::Defer;
    };

    // No lock is held between get_slots_since and fetching the slot meta, so
    // SlotMeta could have been updated in between. If so, continue and the
    // next iteration of generate_new_bank_forks will have consistent values.
    if slot_meta.parent_slot != Some(parent_slot) {
        return ChildBankReplayStart::Defer;
    }

    // Genesis doesn't have a block id.
    if parent_slot != 0 && Some(slot_meta.parent_block_id) != parent_bank.block_id() {
        // There were duplicate blocks in this slot and we have the wrong one
        // replayed. Do not continue down this fork. If this fork ends up
        // being ParentReady, votor will repair the duplicate parent block and
        // perform the switch so replay can continue.
        return ChildBankReplayStart::Defer;
    }

    let num_shreds = u64::from(slot_meta.replay_fec_set_index);
    if !slot_meta.has_update_parent() {
        return ChildBankReplayStart::FromStart;
    }

    info!(
        "slot {child_slot}: replay offset {} shreds from parent {:?}",
        num_shreds, slot_meta.parent_slot
    );
    ChildBankReplayStart::FromUpdateParent(num_shreds)
}

/// Clear an in-progress bank so the next replay iteration recreates it from
/// the current UpdateParent parent and FEC-set offset recorded in `SlotMeta`.
fn try_restart_slot_from_update_parent(
    my_pubkey: &Pubkey,
    blockstore: &Blockstore,
    bank_forks: &RwLock<BankForks>,
    progress: &mut ProgressMap,
    async_verification_freelist: &mut Vec<AsyncVerificationProgress>,
    slot: Slot,
    replay_vote_sender: &ReplayVoteSender,
    migration_status: &MigrationStatus,
    source: &str,
) -> bool {
    if !migration_status.should_allow_fast_leader_handover(slot) {
        return false;
    }
    if blockstore.is_dead(slot) {
        return false;
    }
    let Some(slot_meta) = blockstore.meta(slot).expect("Blockstore should not fail") else {
        return false;
    };
    let Some(replay_fec_set_index) =
        update_parent_replay_offset(slot, &slot_meta, bank_forks.read().unwrap().root())
    else {
        return false;
    };

    // Skip if neither replay bank nor progress exists. If progress exists
    // without a bank, clear it as stale state so the next replay iteration
    // can recreate the bank from the updated SlotMeta.
    let bank = bank_forks.read().unwrap().get(slot);
    if bank.is_none() && !progress.contains_key(&slot) {
        return false;
    }
    if bank
        .as_ref()
        .is_some_and(|bank| ReplayStage::leader_is_me(bank.leader_id(), my_pubkey))
    {
        return false;
    }
    if progress.get(&slot).is_some_and(|progress| {
        progress.dead_reason.is_some()
            && !matches!(
                progress.dead_reason,
                Some(DeadSlotReason::ReplayFailureBeforeUpdateParent)
            )
    }) {
        return false;
    }

    // Check if we've already replayed past the UpdateParent marker.
    let current_num_shreds = progress
        .get(&slot)
        .map(|p| p.replay_progress.read().unwrap().num_shreds)
        .unwrap_or(0);

    if current_num_shreds >= replay_fec_set_index {
        return false;
    }

    // Clear and let generate_new_bank_forks restart from replay_fec_set_index.
    info!(
        "{my_pubkey}: restarting slot {slot} from replay_fec_set_index {replay_fec_set_index} via \
         {source} (was at {current_num_shreds} shreds)",
    );
    if let Some(bank) = bank {
        send_invalid_bank(&bank, replay_vote_sender);
    }
    ReplayStage::clear_banks(
        &BTreeSet::from([slot]),
        bank_forks,
        progress,
        async_verification_freelist,
    );
    true
}

/// Revisit soft-dead slots as more shreds arrive.
///
/// If an UpdateParent marker is now visible in SlotMeta, replay is restarted
/// from the marker's FEC set. If the slot completes without a marker, the
/// soft-dead state is promoted to a durable hard-dead slot.
pub(super) fn process_soft_dead_slots(
    my_pubkey: &Pubkey,
    blockstore: &Arc<Blockstore>,
    bank_forks: &RwLock<BankForks>,
    rpc_subscriptions: &Option<Arc<solana_rpc::rpc_subscriptions::RpcSubscriptions>>,
    slot_status_notifier: &Option<solana_rpc::slot_status_notifier::SlotStatusNotifier>,
    progress: &mut ProgressMap,
    async_verification_freelist: &mut Vec<AsyncVerificationProgress>,
    replay_vote_sender: &ReplayVoteSender,
    migration_status: &MigrationStatus,
) {
    let soft_dead_slots = progress
        .iter()
        .filter_map(|(&slot, progress)| {
            matches!(
                progress.dead_reason,
                Some(DeadSlotReason::ReplayFailureBeforeUpdateParent)
            )
            .then_some(slot)
        })
        .collect::<Vec<_>>();

    for slot in soft_dead_slots {
        if blockstore.is_dead(slot) {
            if let Some(progress) = progress.get_mut(&slot) {
                progress.mark_dead(DeadSlotReason::Hard);
            }
            continue;
        }

        let Some(slot_meta) = blockstore.meta(slot).expect("Blockstore should not fail") else {
            continue;
        };
        let replay_offset =
            update_parent_replay_offset(slot, &slot_meta, bank_forks.read().unwrap().root());

        if replay_offset.is_some() {
            try_restart_slot_from_update_parent(
                my_pubkey,
                blockstore.as_ref(),
                bank_forks,
                progress,
                async_verification_freelist,
                slot,
                replay_vote_sender,
                migration_status,
                "soft-dead scan",
            );
            continue;
        }

        if slot_meta.has_update_parent() || slot_meta.is_full() {
            let (err, log_level) = if slot_meta.has_update_parent() {
                (
                    "error: replay failed before UpdateParent, and the UpdateParent metadata is \
                     invalid"
                        .to_string(),
                    DeadSlotLogLevel::Error,
                )
            } else {
                (
                    "error: replay failed before a usable UpdateParent marker, and the completed \
                     slot cannot replay from UpdateParent"
                        .to_string(),
                    DeadSlotLogLevel::Info,
                )
            };
            let bank = bank_forks.read().unwrap().get(slot);
            if let Some(bank) = bank {
                let notifications = DeadSlotNotifications {
                    blockstore: blockstore.clone(),
                    rpc_subscriptions: rpc_subscriptions.clone(),
                    slot_status_notifier: slot_status_notifier.clone(),
                    replay_vote_sender: replay_vote_sender.clone(),
                };
                mark_hard_dead_slot_notifications(&bank, err, log_level, progress, &notifications);
            }
        }
    }
}

/// Handles UpdateParent signals by clearing slot state so replay restarts from
/// the new parent. Skips live own-leader banks, slots replay has not started,
/// and slots that already replayed past the UpdateParent marker.
pub(super) fn handle_update_parent_interrupts(
    my_pubkey: &Pubkey,
    blockstore: &Blockstore,
    bank_forks: &RwLock<BankForks>,
    progress: &mut ProgressMap,
    async_verification_freelist: &mut Vec<AsyncVerificationProgress>,
    update_parent_receiver: &UpdateParentReceiver,
    replay_vote_sender: &ReplayVoteSender,
    migration_status: &MigrationStatus,
) {
    while let Ok(signal) = update_parent_receiver.try_recv() {
        try_restart_slot_from_update_parent(
            my_pubkey,
            blockstore,
            bank_forks,
            progress,
            async_verification_freelist,
            signal.slot,
            replay_vote_sender,
            migration_status,
            "UpdateParent signal",
        );
    }
}

/// Restart an obsolete optimistic-parent bank after replay reaches the
/// `UpdateParent` marker itself.
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_abandoned_bank(
    process_active_banks_context: &ProcessActiveBanksContext,
    bank: &Arc<Bank>,
    bank_slot: Slot,
    update_parent: &VersionedUpdateParent,
    bank_forks: &Arc<RwLock<BankForks>>,
    progress: &mut ProgressMap,
    async_verification_freelist: &mut Vec<AsyncVerificationProgress>,
    duplicate_slots_to_repair: &mut DuplicateSlotsToRepair,
    purge_repair_slot_counter: &mut PurgeRepairSlotCounter,
    tbft_structs: Option<&mut TowerBFTStructures>,
) {
    // Handle UpdateParent marker during fast leader handover. The leader
    // built on an optimistic parent that didn't match the finalized parent.
    //
    // We clear the bank and remove the progress entry. The bank will be
    // recreated with the correct parent by generate_new_bank_forks, which
    // reads slot meta to determine both the new parent and the correct
    // replay offset (replay_fec_set_index).
    let new_parent_slot = match &update_parent {
        VersionedUpdateParent::V1(x) => x.new_parent_slot,
    };

    datapoint_info!(
        "replay-stage-update-parent",
        ("slot", bank_slot, i64),
        ("old_parent_slot", bank.parent_slot(), i64),
        ("new_parent_slot", new_parent_slot, i64),
    );

    info!(
        "AbandonedBank at slot {bank_slot}: switching parent from {} to {new_parent_slot}",
        bank.parent_slot()
    );

    let update_parent_ready = !process_active_banks_context.blockstore.is_dead(bank_slot)
        && process_active_banks_context
            .blockstore
            .meta(bank_slot)
            .expect("Blockstore operations must succeed")
            .is_some_and(|slot_meta| {
                update_parent_replay_offset(
                    bank_slot,
                    &slot_meta,
                    bank_forks.read().unwrap().root(),
                )
                .is_some()
            });
    if !update_parent_ready {
        let root = bank_forks.read().unwrap().root();
        let mut dead_slot_context = process_active_banks_context.dead_slot_context(
            root,
            duplicate_slots_to_repair,
            purge_repair_slot_counter,
            tbft_structs,
        );
        mark_hard_dead_slot(
            bank,
            "error: replay abandoned bank for an unusable UpdateParent".to_string(),
            DeadSlotLogLevel::Error,
            progress,
            &mut dead_slot_context,
        );
        return;
    }

    send_invalid_bank(bank, &process_active_banks_context.replay_vote_sender);

    // Clear the bank from bank_forks. It will be recreated with the correct
    // parent by generate_new_bank_forks on the next iteration.
    ReplayStage::clear_banks(
        &BTreeSet::from([bank_slot]),
        bank_forks.as_ref(),
        progress,
        async_verification_freelist,
    );
}
