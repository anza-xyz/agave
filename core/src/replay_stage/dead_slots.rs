//! Dead-slot classification and notification helpers for replay.

use {
    crate::consensus::progress_map::{DeadSlotReason, ProgressMap},
    solana_clock::Slot,
    solana_ledger::{
        block_error::BlockError,
        blockstore::{Blockstore, BlockstoreError},
        blockstore_processor::BlockstoreProcessorError,
    },
    solana_rpc::{rpc_subscriptions::RpcSubscriptions, slot_status_notifier::SlotStatusNotifier},
    solana_rpc_client_api::response::SlotUpdate,
    solana_runtime::{bank::Bank, block_component_processor::BlockComponentProcessorError},
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
            BlockstoreProcessorError::FailedToLoadEntries(BlockstoreError::BlockAborted(_)) => {
                Self::Info
            }
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
}

/// All state needed to decide and publish a replay dead-slot transition.
pub(super) struct DeadSlotContext {
    /// Persistent/RPC notification sinks.
    pub(super) notifications: DeadSlotNotifications,
}

/// Replay errors that may be caused solely by executing the optimistic-parent prefix.
fn is_update_parent_recoverable_replay_error(err: &BlockstoreProcessorError) -> bool {
    match err {
        BlockstoreProcessorError::InvalidBlock(_)
        | BlockstoreProcessorError::InvalidTransaction(_) => true,
        BlockstoreProcessorError::FailedToLoadEntries(
            BlockstoreError::InvalidShredData(_) | BlockstoreError::BlockAborted(_),
        ) => true,
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

/// Returns the recoverable dead-slot reason while the slot's final parent is
/// not yet known.
///
/// Fast leader handover may execute a prefix built on an optimistic parent. If
/// a later `UpdateParent` marker can still make that prefix obsolete, the slot
/// is only soft-dead in `ProgressMap`; blockstore/RPC are updated only after the
/// marker proves recovery impossible.
///
/// Returning `None` means the failure is not eligible for soft-dead recovery;
/// `mark_replay_dead_slot` will therefore mark the slot hard-dead.
fn soft_dead_reason(
    blockstore: &Blockstore,
    bank: &Bank,
    err: &BlockstoreProcessorError,
    progress: &ProgressMap,
) -> Option<DeadSlotReason> {
    let slot = bank.slot();
    if !bank.feature_set.snapshot().alpenglow_fast_leader_handover
        || blockstore.is_dead(slot)
        || !is_update_parent_recoverable_replay_error(err)
    {
        return None;
    }

    let fork_progress = progress.get(&slot)?;
    let replayed_shreds = fork_progress.replay_progress.read().unwrap().num_shreds;
    let slot_meta = blockstore.meta(slot).ok().flatten()?;

    if slot_meta.has_update_parent() {
        let replay_fec_set_index = u64::from(slot_meta.replay_fec_set_index);
        if replayed_shreds < replay_fec_set_index {
            return Some(DeadSlotReason::ReplayFailureBeforeUpdateParent);
        }

        // SpuriousUpdateParent is raised before the marker is consumed. At
        // equality, replay therefore has not executed past the new-parent
        // boundary and can safely restart from it. Other failures at the same
        // boundary remain terminal.
        if replayed_shreds == replay_fec_set_index
            && matches!(
                err,
                BlockstoreProcessorError::BlockComponentProcessor(
                    BlockComponentProcessorError::SpuriousUpdateParent
                )
            )
        {
            return Some(DeadSlotReason::ReplayFailureAtUpdateParent(
                replay_fec_set_index,
            ));
        }

        // Execution error at or past UpdateParent is otherwise hard-dead,
        // because the marker can no longer make the replayed prefix obsolete.
        None
    } else if slot_meta.is_full() {
        // The slot is complete and no `UpdateParent` marker was observed, so
        // the replay failure is terminal.
        None
    } else {
        Some(DeadSlotReason::ReplayFailureBeforeUpdateParent)
    }
}

/// Mark a slot soft-dead after replay fails before a possible UpdateParent.
///
/// This intentionally avoids blockstore/RPC dead-slot writes because a later
/// marker may restart replay from the updated parent.
fn mark_soft_dead_slot(
    bank: &Bank,
    err: &BlockstoreProcessorError,
    reason: DeadSlotReason,
    progress: &mut ProgressMap,
) {
    let slot = bank.slot();
    datapoint_info!(
        "replay-stage-soft_dead_slot",
        ("error", format!("error: {err:?}"), String),
        ("slot", slot, i64),
    );
    progress.get_mut(&slot).unwrap().mark_dead(reason);
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
    // Keep the entry so replay and repair can observe the terminal state.
    progress
        .get_mut(&slot)
        .unwrap()
        .mark_dead(DeadSlotReason::Hard);
    notifications
        .blockstore
        .set_dead_slot(slot)
        .expect("Failed to mark slot as dead in blockstore");
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
    dead_slot_context: &DeadSlotContext,
) {
    mark_hard_dead_slot_notifications(
        bank,
        err,
        log_level,
        progress,
        &dead_slot_context.notifications,
    );
}

/// Classify a replay failure as soft-dead or hard-dead and publish the
/// appropriate state transition.
pub(super) fn mark_replay_dead_slot(
    bank: &Bank,
    err: &BlockstoreProcessorError,
    progress: &mut ProgressMap,
    dead_slot_context: &DeadSlotContext,
) {
    bank.clear_accounts_lt_hash_async_progress_is_at_end();
    if let Some(reason) = soft_dead_reason(
        dead_slot_context.notifications.blockstore.as_ref(),
        bank,
        err,
        progress,
    ) {
        mark_soft_dead_slot(bank, err, reason, progress);
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
