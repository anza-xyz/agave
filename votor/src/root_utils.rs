use {
    crate::{
        commitment::{CommitmentType, update_commitment_cache},
        common::nonblocking_send,
        event_handler::PendingBlocks,
        voting_utils::VotingContext,
        votor::SharedContext,
    },
    agave_votor_messages::consensus_message::Block,
    crossbeam_channel::Sender,
    solana_clock::Slot,
    solana_hash::Hash,
    solana_ledger::{blockstore::Blockstore, leader_schedule_cache::LeaderScheduleCache},
    solana_pubkey::Pubkey,
    solana_rpc::{
        optimistically_confirmed_bank_tracker::{BankNotification, BankNotificationSenderConfig},
        rpc_subscriptions::RpcSubscriptions,
    },
    solana_runtime::{
        bank_forks::BankForks, bank_forks_controller::BankForksController,
        installed_scheduler_pool::BankWithScheduler, snapshot_controller::SnapshotController,
    },
    solana_time_utils::timestamp,
    std::{
        collections::BTreeSet,
        sync::{Arc, RwLock},
    },
};

/// Structures that are not used in the event loop, but need to be updated
/// or notified when setting root
pub(crate) struct RootContext {
    pub(crate) bank_notification_sender: Option<BankNotificationSenderConfig>,
    pub(crate) bank_forks_controller: Arc<dyn BankForksController>,
}

/// Sets the root for the votor event handling loop. Handles rooting all things
/// except the certificate pool
pub(crate) fn set_root(
    my_pubkey: &Pubkey,
    new_root: Block,
    bank_hash: Hash,
    ctx: &SharedContext,
    vctx: &mut VotingContext,
    rctx: &RootContext,
    pending_blocks: &mut PendingBlocks,
    finalized_blocks: &mut BTreeSet<Block>,
    received_shred: &mut BTreeSet<Slot>,
) {
    let new_root_slot = new_root.slot;
    info!("{my_pubkey}: setting root {new_root:?}");
    vctx.vote_history.set_root(new_root_slot);
    *pending_blocks = pending_blocks.split_off(&new_root_slot);
    *finalized_blocks = finalized_blocks.split_off(&Block {
        slot: new_root_slot,
        block_id: Hash::default(),
    });
    *received_shred = received_shred.split_off(&new_root_slot);

    rctx.bank_forks_controller.enqueue_set_root(new_root);

    if let Err(e) = ctx.blockstore.insert_optimistic_slot(
        new_root_slot,
        &bank_hash,
        timestamp().try_into().unwrap(),
    ) {
        error!("failed to record optimistic slot in blockstore: slot={new_root_slot}: {e:?}");
    }

    let dependency_work = rctx
        .bank_notification_sender
        .as_ref()
        .and_then(|config| config.dependency_tracker.as_ref())
        .map(|tracker| tracker.get_current_declared_work());

    update_commitment_cache(
        my_pubkey,
        CommitmentType::Rooted,
        new_root_slot,
        dependency_work,
        &vctx.commitment_sender,
    );

    // It is critical to send the OC notification in order to keep compatibility with
    // the RPC API. Additionally the PrioritizationFeeCache relies on this notification
    // in order to perform cleanup. In the future we will look to deprecate OC and remove
    // these code paths.
    if let Some(config) = &rctx.bank_notification_sender
        && let Err(chanel_name) = nonblocking_send(
            my_pubkey,
            &config.sender,
            (
                BankNotification::OptimisticallyConfirmed(new_root_slot, bank_hash),
                dependency_work,
            ),
            "bank_notification_sender",
        )
    {
        info!("{my_pubkey}: channel {chanel_name} disconnected");
    }
}

/// Sets the new root, additionally performs the callback after setting the bank forks root
/// During this transition period where both replay stage and voting loop can root depending on the feature flag we
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
) where
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
    let rooted_slot_notifications = bank_notification_sender
        .as_ref()
        .is_some_and(|sender| sender.should_send_parents)
        .then(|| {
            let new_chain = rooted_banks
                .iter()
                .map(|bank| (bank.slot(), bank.bank_id()))
                .collect();
            (new_chain, oldest_parent.unwrap_or(parent_slot))
        });

    // Call leader schedule_cache.set_root() before blockstore.set_root() because
    // bank_forks.root is consumed by repair_service to update gossip, so we don't want to
    // get shreds for repair on gossip before we update leader schedule, otherwise they may
    // get dropped.
    leader_schedule_cache.set_root(rooted_banks.last().unwrap());
    blockstore
        .set_roots(rooted_slots.iter())
        .expect("Ledger set roots failed");
    set_bank_forks_root(
        my_pubkey,
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
        if let Err(channel_name) = nonblocking_send(
            my_pubkey,
            &sender.sender,
            (BankNotification::NewRootBank(root_bank), dependency_work),
            "bank_notification_sender",
        ) {
            info!("{my_pubkey} channel {channel_name} disconnected");
        }
        if let Some((new_chain, oldest_parent)) = rooted_slot_notifications {
            let dependency_work = sender
                .dependency_tracker
                .as_ref()
                .map(|s| s.get_current_declared_work());
            if let Err(channel_name) = nonblocking_send(
                my_pubkey,
                &sender.sender,
                (
                    BankNotification::NewRootedChain(new_chain, oldest_parent),
                    dependency_work,
                ),
                "bank_notification_sender",
            ) {
                info!("{my_pubkey} channel {channel_name} disconnected");
            }
        }
    }
    info!("{my_pubkey}: new root {new_root}");
}

/// Sets the bank forks root:
/// - Quiesce and synchronously purge banks orphaned by the new root
/// - Prune the program cache
/// - Prune bank forks and drop the removed banks
/// - Calls the callback for use in replay stage and tests
pub fn set_bank_forks_root<CB>(
    my_pubkey: &Pubkey,
    new_root: Slot,
    bank_forks: &RwLock<BankForks>,
    snapshot_controller: Option<&SnapshotController>,
    highest_super_majority_root: Option<Slot>,
    drop_bank_sender: &Sender<Vec<BankWithScheduler>>,
    callback: CB,
) where
    CB: FnOnce(&BankForks),
{
    let banks_to_remove: Vec<_> = {
        let bank_forks = bank_forks.read().unwrap();
        let old_root = bank_forks.root();
        let new_root_ancestors = bank_forks
            .get(new_root)
            .expect("Root bank doesn't exist")
            .proper_ancestors_set();
        bank_forks
            .get_non_rooted(new_root, highest_super_majority_root)
            .filter_map(|slot| {
                bank_forks.get_with_scheduler(slot).map(|bank| {
                    let is_orphaned = slot > old_root && !new_root_ancestors.contains(&slot);
                    (bank, is_orphaned)
                })
            })
            .collect()
    };

    let mut orphaned_slot_bank_ids = Vec::new();
    for (bank, is_orphaned) in &banks_to_remove {
        // Quiesce all banks whose shared slot state will be purged before waiting on schedulers.
        // Banks removed because they are rooted ancestors are already frozen and do not need the
        // retirement barrier.
        if *is_orphaned {
            bank.quiesce_transaction_execution();
            orphaned_slot_bank_ids.push((bank.slot(), bank.bank_id()));
        }
    }
    for (bank, _) in &banks_to_remove {
        let _ = bank.wait_for_completed_scheduler();
    }

    if !orphaned_slot_bank_ids.is_empty() {
        // Purge these banks inline so we are not waiting on the lazy Accounts Background Service for cleanup.
        // Waiting could stall replay if we recreate a bank for this slot.
        let root_bank = bank_forks.read().unwrap().root_bank();
        root_bank.remove_unrooted_slots(&orphaned_slot_bank_ids);
        for (slot, _) in &orphaned_slot_bank_ids {
            root_bank.clear_slot_signatures(*slot);
            root_bank.prune_program_cache_by_deployment_slot(*slot);
        }
    }

    bank_forks.read().unwrap().prune_program_cache(new_root);
    let removed_banks = bank_forks.write().unwrap().set_root(
        new_root,
        snapshot_controller,
        highest_super_majority_root,
    );

    if let Err(channel_name) = nonblocking_send(
        my_pubkey,
        drop_bank_sender,
        removed_banks,
        "drop_bank_sender",
    ) {
        info!("{my_pubkey} channel {channel_name} disconnected");
    }
    let r_bank_forks = bank_forks.read().unwrap();
    callback(&r_bank_forks);
}
