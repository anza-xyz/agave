use {
    crate::{event_handler::PendingBlocks, voting_utils::VotingContext, votor::SharedContext},
    agave_votor_messages::consensus_message::Block,
    crossbeam_channel::{SendError, Sender},
    log::{info, warn},
    solana_clock::Slot,
    solana_ledger::{
        blockstore::{Blockstore, BlockstoreError},
        leader_schedule_cache::LeaderScheduleCache,
    },
    solana_pubkey::Pubkey,
    solana_rpc::{
        optimistically_confirmed_bank_tracker::{BankNotification, BankNotificationSenderConfig},
        rpc_subscriptions::RpcSubscriptions,
    },
    solana_runtime::{
        bank_forks::BankForks, installed_scheduler_pool::BankWithScheduler,
        snapshot_controller::SnapshotController,
    },
    solana_time_utils::timestamp,
    std::{
        collections::BTreeSet,
        sync::{Arc, RwLock},
    },
    thiserror::Error,
};

#[allow(dead_code)]
/// Structures that are not used in the event loop, but need to be updated
/// or notified when setting root
pub(crate) struct RootContext {
    pub(crate) leader_schedule_cache: Arc<LeaderScheduleCache>,
    pub(crate) snapshot_controller: Option<Arc<SnapshotController>>,
    pub(crate) bank_notification_sender: Option<BankNotificationSenderConfig>,
    pub(crate) drop_bank_sender: Sender<Vec<BankWithScheduler>>,
}

#[derive(Debug, Error)]
pub enum SetRootError {
    #[error("Failed to record slot in blockstore: {0}")]
    Blockstore(#[from] BlockstoreError),

    #[error("Error sending bank nofification: {0}")]
    SendNotification(#[from] SendError<()>),
}

/// Sets the root for the votor event handling loop. Handles rooting all things
/// except the certificate pool
pub(crate) fn set_root(
    my_pubkey: &Pubkey,
    new_root: Slot,
    ctx: &SharedContext,
    vctx: &mut VotingContext,
    rctx: &RootContext,
    pending_blocks: &mut PendingBlocks,
    finalized_blocks: &mut BTreeSet<Block>,
    received_shred: &mut BTreeSet<Slot>,
) -> Result<(), SetRootError> {
    info!("{my_pubkey}: setting root {new_root}");
    vctx.vote_history.set_root(new_root);
    pending_blocks.retain(|pending_block, _| *pending_block >= new_root);
    finalized_blocks.retain(|(slot, _)| *slot >= new_root);
    received_shred.retain(|slot| *slot >= new_root);

    check_and_handle_new_root(
        new_root,
        new_root,
        rctx.snapshot_controller.as_deref(),
        Some(new_root),
        &rctx.bank_notification_sender,
        &rctx.drop_bank_sender,
        &ctx.blockstore,
        &rctx.leader_schedule_cache,
        &ctx.bank_forks,
        ctx.rpc_subscriptions.as_deref(),
        my_pubkey,
        |_| {},
    )?;

    // Distinguish between duplicate versions of same slot
    let hash = ctx.bank_forks.read().unwrap().bank_hash(new_root).unwrap();
    ctx.blockstore
        .insert_optimistic_slot(new_root, &hash, timestamp().try_into().unwrap())?;

    // It is critical to send the OC notification in order to keep compatibility with
    // the RPC API. Additionally the PrioritizationFeeCache relies on this notification
    // in order to perform cleanup. In the future we will look to deprecate OC and remove
    // these code paths.
    if let Some(config) = &rctx.bank_notification_sender {
        let dependency_work = config
            .dependency_tracker
            .as_ref()
            .map(|s| s.get_current_declared_work());
        config
            .sender
            .send((
                BankNotification::OptimisticallyConfirmed(new_root),
                dependency_work,
            ))
            .map_err(|_| SendError(()))?;
    }

    Ok(())
}

/// Sets the new root, additionally performs the callback after setting the bank forks root
/// During this transition period where both replay stage and votor can root depending on the feature flag we
/// have a callback that cleans up progress map and other tower bft structures. Then the callgraph is
///
/// ReplayStage::check_and_handle_new_root -> root_utils::check_and_handle_new_root(callback)
///                                                             |
///                                                             v
/// ReplayStage::handle_new_root           -> root_utils::set_bank_forks_root(callback) -> callback()
///
/// Votor does not need the progress map or other tower bft structures, so it will not use the callback.
#[allow(clippy::too_many_arguments)]
pub fn check_and_handle_new_root<CB>(
    parent_slot: Slot,
    new_root: Slot,
    snapshot_controller: Option<&SnapshotController>,
    highest_super_majority_root: Option<Slot>,
    bank_notification_sender: &Option<BankNotificationSenderConfig>,
    drop_bank_sender: &Sender<Vec<BankWithScheduler>>,
    blockstore: &Blockstore,
    leader_schedule_cache: &Arc<LeaderScheduleCache>,
    bank_forks: &RwLock<BankForks>,
    rpc_subscriptions: Option<&RpcSubscriptions>,
    my_pubkey: &Pubkey,
    callback: CB,
) -> Result<(), SetRootError>
where
    CB: FnOnce(&BankForks),
{
    // get the root bank before squash
    let root_bank = bank_forks
        .read()
        .unwrap()
        .get(new_root)
        .expect("Root bank doesn't exist");
    let mut rooted_banks = root_bank.parents();
    let oldest_parent = rooted_banks.last().map(|last| last.parent_slot());
    rooted_banks.push(root_bank.clone());
    let rooted_slots: Vec<_> = rooted_banks.iter().map(|bank| bank.slot()).collect();
    // The following differs from rooted_slots by including the parent slot of the oldest parent bank.
    let rooted_slots_with_parents = bank_notification_sender
        .as_ref()
        .is_some_and(|sender| sender.should_send_parents)
        .then(|| {
            let mut new_chain = rooted_slots.clone();
            new_chain.push(oldest_parent.unwrap_or(parent_slot));
            new_chain
        });

    // Call leader schedule_cache.set_root() before blockstore.set_root() because
    // bank_forks.root is consumed by repair_service to update gossip, so we don't want to
    // get shreds for repair on gossip before we update leader schedule, otherwise they may
    // get dropped.
    leader_schedule_cache.set_root(rooted_banks.last().unwrap());
    blockstore.set_roots(rooted_slots.iter())?;

    set_bank_forks_root(
        new_root,
        bank_forks,
        snapshot_controller,
        highest_super_majority_root,
        drop_bank_sender,
        callback,
    );
    blockstore.slots_stats.mark_rooted(new_root);
    if let Some(rpc_subscriptions) = rpc_subscriptions {
        rpc_subscriptions.notify_roots(rooted_slots);
    }
    if let Some(sender) = bank_notification_sender {
        let dependency_work = sender
            .dependency_tracker
            .as_ref()
            .map(|s| s.get_current_declared_work());
        sender
            .sender
            .send((BankNotification::NewRootBank(root_bank), dependency_work))
            .unwrap_or_else(|err| warn!("bank_notification_sender failed: {err:?}"));

        if let Some(new_chain) = rooted_slots_with_parents {
            let dependency_work = sender
                .dependency_tracker
                .as_ref()
                .map(|s| s.get_current_declared_work());
            sender
                .sender
                .send((BankNotification::NewRootedChain(new_chain), dependency_work))
                .unwrap_or_else(|err| warn!("bank_notification_sender failed: {err:?}"));
        }
    }
    info!("{my_pubkey}: new root {new_root}");

    Ok(())
}

/// Sets the bank forks root:
/// - Prune the program cache
/// - Prune bank forks and drop the removed banks
/// - Calls the callback for use in replay stage and tests
pub fn set_bank_forks_root<CB>(
    new_root: Slot,
    bank_forks: &RwLock<BankForks>,
    snapshot_controller: Option<&SnapshotController>,
    highest_super_majority_root: Option<Slot>,
    drop_bank_sender: &Sender<Vec<BankWithScheduler>>,
    callback: CB,
) where
    CB: FnOnce(&BankForks),
{
    bank_forks.read().unwrap().prune_program_cache(new_root);
    let removed_banks = bank_forks.write().unwrap().set_root(
        new_root,
        snapshot_controller,
        highest_super_majority_root,
    );

    drop_bank_sender
        .send(removed_banks)
        .unwrap_or_else(|err| warn!("bank drop failed: {err:?}"));

    let r_bank_forks = bank_forks.read().unwrap();
    callback(&r_bank_forks);
}
