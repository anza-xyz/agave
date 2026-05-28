//! Dead-slot classification and notification helpers for replay.

use {
    super::TowerBFTStructures,
    crate::{
        consensus::progress_map::{DeadSlotReason, ProgressMap},
        repair::{
            ancestor_hashes_service::AncestorHashesReplayUpdateSender,
            cluster_slot_state_verifier::{
                DeadState, DuplicateSlotsToRepair, DuplicateState, PurgeRepairSlotCounter,
                SlotStateUpdate, check_slot_agrees_with_cluster,
            },
        },
    },
    agave_votor_messages::migration::MigrationStatus,
    solana_clock::Slot,
    solana_ledger::{
        block_error::BlockError, blockstore::Blockstore,
        blockstore_processor::BlockstoreProcessorError,
    },
    solana_rpc::{rpc_subscriptions::RpcSubscriptions, slot_status_notifier::SlotStatusNotifier},
    solana_rpc_client_api::response::SlotUpdate,
    solana_runtime::{
        bank::Bank,
        vote_sender_types::{ReplayVoteMessage, ReplayVoteSender},
    },
    solana_time_utils::timestamp,
    std::sync::Arc,
};

/// Controls the severity used for hard-dead-slot telemetry.
///
/// Some terminal replay outcomes are expected protocol behavior, such as a
/// leader abandoning a block with too few ticks. Those still make the slot dead,
/// but should not alert operators as validator errors.
#[derive(Clone, Copy)]
pub(super) enum DeadSlotLogLevel {
    Info,
    Error,
}

impl DeadSlotLogLevel {
    /// Pick the operator-facing severity for a replay failure.
    pub(super) fn for_replay_error(err: &BlockstoreProcessorError) -> Self {
        match err {
            BlockstoreProcessorError::InvalidBlock(BlockError::TooFewTicks) => Self::Info,
            _ => Self::Error,
        }
    }

    /// Emit the standard dead-slot datapoint with the selected severity.
    pub(super) fn datapoint(self, slot: Slot, err: String) {
        match self {
            Self::Info => {
                datapoint_info!(
                    "replay-stage-mark_dead_slot",
                    ("error", err, String),
                    ("slot", slot, i64)
                );
            }
            Self::Error => {
                datapoint_error!(
                    "replay-stage-mark_dead_slot",
                    ("error", err, String),
                    ("slot", slot, i64)
                );
            }
        }
    }
}

/// Sinks that observe replay dead-slot transitions.
pub(super) struct DeadSlotNotifications {
    /// Persistent ledger state. Hard-dead slots are written here; soft-dead
    /// slots intentionally are not.
    pub(super) blockstore: Arc<Blockstore>,
    /// RPC subscription fanout for durable dead-slot notifications.
    pub(super) rpc_subscriptions: Option<Arc<RpcSubscriptions>>,
    /// Optional test/runtime hook used by slot-status observers.
    pub(super) slot_status_notifier: Option<SlotStatusNotifier>,
    /// Replay-vote channel used to release buffered votes for the failed bank.
    pub(super) replay_vote_sender: ReplayVoteSender,
}

/// Tower/duplicate bookkeeping that must be updated only for hard-dead slots.
pub(super) struct DeadSlotDuplicateContext<'a> {
    /// Current root used by the duplicate-slot agreement state machine.
    pub(super) root: Slot,
    /// Slots that should be repaired after duplicate/dead state transitions.
    pub(super) duplicate_slots_to_repair: &'a mut DuplicateSlotsToRepair,
    /// Notifies ancestor-hash repair when a dead slot changes duplicate state.
    pub(super) ancestor_hashes_replay_update_sender: &'a AncestorHashesReplayUpdateSender,
    /// Rate-limits purge repair requests triggered by duplicate/dead handling.
    pub(super) purge_repair_slot_counter: &'a mut PurgeRepairSlotCounter,
    /// Tower data is present only on the Tower replay path; Alpenglow replay
    /// passes `None` because it does not run this duplicate state machine.
    pub(super) tbft_structs: Option<&'a mut TowerBFTStructures>,
}

/// All state needed to decide and publish a replay dead-slot transition.
pub(super) struct DeadSlotContext<'a> {
    /// Persistent/RPC/replay-vote notification sinks.
    pub(super) notifications: DeadSlotNotifications,
    /// Duplicate/Tower bookkeeping for hard-dead transitions.
    pub(super) duplicate: DeadSlotDuplicateContext<'a>,
    /// Migration gates decide whether an UpdateParent marker can still recover
    /// a failed prefix by restarting replay from a later FEC set.
    pub(super) migration_status: &'a MigrationStatus,
}

/// Replay errors that may be caused solely by executing the optimistic-parent prefix.
fn is_update_parent_recoverable_replay_error(err: &BlockstoreProcessorError) -> bool {
    match err {
        BlockstoreProcessorError::InvalidBlock(_)
        | BlockstoreProcessorError::InvalidTransaction(_)
        | BlockstoreProcessorError::UserTransactionsInVoteOnlyBank(_)
        | BlockstoreProcessorError::ChainedBlockIdFailure(_, _) => true,
        BlockstoreProcessorError::BlockComponentProcessor(err) => {
            err.is_update_parent_recoverable_replay_error()
        }
        BlockstoreProcessorError::FailedToLoadEntries(_)
        | BlockstoreProcessorError::FailedToLoadMeta
        | BlockstoreProcessorError::FailedToReplayBank0
        | BlockstoreProcessorError::NoValidForksFound
        | BlockstoreProcessorError::InvalidHardFork(_)
        | BlockstoreProcessorError::BankHashMismatch(..)
        | BlockstoreProcessorError::RootBankWithMismatchedCapitalization(_) => false,
    }
}

/// Returns true when replay failed before the slot's final parent is known.
///
/// Fast leader handover may execute a prefix built on an optimistic parent. If
/// a later `UpdateParent` marker can still make that prefix obsolete, the slot
/// is only soft-dead in `ProgressMap`; blockstore/RPC are updated only after the
/// marker proves recovery impossible.
fn should_mark_soft_dead(
    blockstore: &Blockstore,
    bank: &Bank,
    err: &BlockstoreProcessorError,
    progress: &ProgressMap,
    migration_status: &MigrationStatus,
) -> bool {
    let slot = bank.slot();
    if !migration_status.should_allow_fast_leader_handover(slot)
        || blockstore.is_dead(slot)
        || !is_update_parent_recoverable_replay_error(err)
    {
        return false;
    }

    let Some(fork_progress) = progress.get(&slot) else {
        return false;
    };
    let replayed_shreds = fork_progress.replay_progress.read().unwrap().num_shreds;
    let Some(slot_meta) = blockstore.meta(slot).ok().flatten() else {
        return false;
    };

    if slot_meta.has_update_parent() {
        if replayed_shreds >= u64::from(slot_meta.replay_fec_set_index) {
            // Execution error past update parent is hard-dead, because the
            // marker can no longer make the replayed prefix obsolete.
            return false;
        }
    } else if slot_meta.is_full() {
        // The slot is complete and no `UpdateParent` marker was observed, so
        // the replay failure is terminal.
        return false;
    }

    true
}

/// Mark a slot soft-dead after replay fails before a possible UpdateParent.
///
/// This releases replay-vote state for the current bank, but intentionally
/// avoids blockstore/RPC dead-slot writes because a later marker may restart
/// replay from the updated parent.
fn mark_soft_dead_slot(
    bank: &Bank,
    err: &BlockstoreProcessorError,
    progress: &mut ProgressMap,
    notifications: &DeadSlotNotifications,
) {
    let slot = bank.slot();
    datapoint_info!(
        "replay-stage-soft_dead_slot",
        ("error", format!("error: {err:?}"), String),
        ("slot", slot, i64),
    );
    progress
        .get_mut(&slot)
        .unwrap()
        .mark_dead(DeadSlotReason::ReplayFailureBeforeUpdateParent);
    send_invalid_bank(bank, &notifications.replay_vote_sender);
}

/// Release replay-vote state for a bank that replay will no longer finish.
pub(super) fn send_invalid_bank(bank: &Bank, replay_vote_sender: &ReplayVoteSender) {
    replay_vote_sender
        .send(ReplayVoteMessage::InvalidBank {
            replay_bank_id: bank.bank_id(),
            replay_slot: bank.slot(),
        })
        .expect("Failed to send InvalidBank message to replay vote sender");
}

/// Persist and publish a hard-dead slot.
///
/// Unlike soft-dead slots, hard-dead slots are terminal locally: the slot is
/// written dead in blockstore and observers are notified.
pub(super) fn mark_hard_dead_slot_notifications(
    bank: &Bank,
    err: String,
    log_level: DeadSlotLogLevel,
    progress: &mut ProgressMap,
    notifications: &DeadSlotNotifications,
) {
    let slot = bank.slot();
    let parent_slot = bank.parent_slot();
    log_level.datapoint(slot, err.clone());
    // Do not remove from progress map when marking dead! Needed by
    // `process_duplicate_confirmed_slots()`
    progress
        .get_mut(&slot)
        .unwrap()
        .mark_dead(DeadSlotReason::Hard);
    notifications
        .blockstore
        .set_dead_slot(slot)
        .expect("Failed to mark slot as dead in blockstore");
    send_invalid_bank(bank, &notifications.replay_vote_sender);

    notifications.blockstore.slots_stats.mark_dead(slot);

    if let Some(slot_status_notifier) = notifications.slot_status_notifier.as_ref() {
        slot_status_notifier
            .read()
            .unwrap()
            .notify_slot_dead(slot, parent_slot, err.clone());
    }

    if let Some(rpc_subscriptions) = notifications.rpc_subscriptions.as_ref() {
        rpc_subscriptions.notify_slot_update(SlotUpdate::Dead {
            slot,
            err,
            timestamp: timestamp(),
        });
    }
}

pub(super) fn mark_hard_dead_slot(
    bank: &Bank,
    err: String,
    log_level: DeadSlotLogLevel,
    progress: &mut ProgressMap,
    dead_slot_context: &mut DeadSlotContext<'_>,
) {
    let slot = bank.slot();
    mark_hard_dead_slot_notifications(
        bank,
        err,
        log_level,
        progress,
        &dead_slot_context.notifications,
    );

    if let Some(TowerBFTStructures {
        heaviest_subtree_fork_choice,
        duplicate_slots_tracker,
        duplicate_confirmed_slots,
        epoch_slots_frozen_slots,
        ..
    }) = dead_slot_context.duplicate.tbft_structs.as_deref_mut()
    {
        let dead_state = DeadState::new_from_state(
            slot,
            duplicate_slots_tracker,
            duplicate_confirmed_slots,
            heaviest_subtree_fork_choice,
            epoch_slots_frozen_slots,
        );
        check_slot_agrees_with_cluster(
            slot,
            dead_slot_context.duplicate.root,
            dead_slot_context.notifications.blockstore.as_ref(),
            duplicate_slots_tracker,
            epoch_slots_frozen_slots,
            heaviest_subtree_fork_choice,
            dead_slot_context.duplicate.duplicate_slots_to_repair,
            dead_slot_context
                .duplicate
                .ancestor_hashes_replay_update_sender,
            dead_slot_context.duplicate.purge_repair_slot_counter,
            SlotStateUpdate::Dead(dead_state),
        );

        // If we previously marked this slot as duplicate in blockstore, let the state machine know.
        if !duplicate_slots_tracker.contains(&slot)
            && dead_slot_context
                .notifications
                .blockstore
                .get_duplicate_slot(slot)
                .is_some()
        {
            let duplicate_state = DuplicateState::new_from_state(
                slot,
                duplicate_confirmed_slots,
                heaviest_subtree_fork_choice,
                || true,
                || None,
            );
            check_slot_agrees_with_cluster(
                slot,
                dead_slot_context.duplicate.root,
                dead_slot_context.notifications.blockstore.as_ref(),
                duplicate_slots_tracker,
                epoch_slots_frozen_slots,
                heaviest_subtree_fork_choice,
                dead_slot_context.duplicate.duplicate_slots_to_repair,
                dead_slot_context
                    .duplicate
                    .ancestor_hashes_replay_update_sender,
                dead_slot_context.duplicate.purge_repair_slot_counter,
                SlotStateUpdate::Duplicate(duplicate_state),
            );
        }
    }
}

/// Classify a replay failure as soft-dead or hard-dead and publish the
/// appropriate state transition.
pub(super) fn mark_replay_dead_slot(
    bank: &Bank,
    err: &BlockstoreProcessorError,
    progress: &mut ProgressMap,
    dead_slot_context: &mut DeadSlotContext<'_>,
) {
    if should_mark_soft_dead(
        dead_slot_context.notifications.blockstore.as_ref(),
        bank,
        err,
        progress,
        dead_slot_context.migration_status,
    ) {
        mark_soft_dead_slot(bank, err, progress, &dead_slot_context.notifications);
        return;
    }

    mark_hard_dead_slot(
        bank,
        format!("error: {err:?}"),
        DeadSlotLogLevel::for_replay_error(err),
        progress,
        dead_slot_context,
    );
}
