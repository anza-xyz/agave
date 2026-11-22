use {
    crate::{
        common::nonblocking_send, event_handler::PendingBlocks, voting_utils::VotingContext,
        votor::SharedContext,
    },
    agave_votor_messages::consensus_message::{Block, BlockId},
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
        transaction_execution::TransactionStatusSender,
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
        block_id: BlockId::default(),
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

    // It is critical to send the OC notification in order to keep compatibility with
    // the RPC API. Additionally the PrioritizationFeeCache relies on this notification
    // in order to perform cleanup. In the future we will look to deprecate OC and remove
    // these code paths.
    if let Some(config) = &rctx.bank_notification_sender {
        let dependency_work = config
            .dependency_tracker
            .as_ref()
            .map(|s| s.get_current_declared_work());
        if let Err(chanel_name) = nonblocking_send(
            my_pubkey,
            &config.sender,
            (
                BankNotification::OptimisticallyConfirmed(new_root_slot, bank_hash),
                dependency_work,
            ),
            "bank_notification_sender",
        ) {
            info!("{my_pubkey}: channel {chanel_name} disconnected");
        }
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
    transaction_status_sender: Option<&TransactionStatusSender>,
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
    if let Some(transaction_status_sender) = transaction_status_sender {
        transaction_status_sender.send_transaction_status_rooted_message(&root_bank);
    }
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
        // Clear these banks' slot-keyed caches inline so we are not waiting on the lazy Accounts
        // Background Service. Waiting could stall replay if we recreate a bank for this slot.
        let root_bank = bank_forks.read().unwrap().root_bank();
        root_bank.mark_banks_removed(&orphaned_slot_bank_ids);
        for (slot, _) in &orphaned_slot_bank_ids {
            root_bank.clear_slot_signatures(*slot);
            root_bank.prune_program_cache_by_deployment_slot(*slot);
        }
    }

    bank_forks.read().unwrap().root_program_cache(new_root);
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

#[cfg(test)]
mod tests {
    use {
        super::*,
        crossbeam_channel::unbounded,
        solana_account::{AccountSharedData, state_traits::StateMutWincode as _},
        solana_keypair::Keypair,
        solana_loader_v3_interface::state::UpgradeableLoaderState,
        solana_program_binaries::bpf_loader_upgradeable_program_accounts,
        solana_rent::Rent,
        solana_runtime::{
            bank::{Bank, SlotLeader, test_utils::goto_end_of_slot},
            genesis_utils::{GenesisConfigInfo, create_genesis_config},
            loader_utils::create_invoke_instruction,
            program_cache_utils::{EntryType, ExpectedEntry, assert_cache_contents, current_env},
        },
        solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
        solana_signer::Signer,
        solana_svm::{
            transaction_error_metrics::TransactionErrorMetrics,
            transaction_processor::{ExecutionRecordingConfig, TransactionProcessingConfig},
        },
        solana_svm_timings::ExecuteTimings,
        solana_transaction::Transaction,
        solana_transaction_error::TransactionError,
        test_case::test_case,
    };

    const RETURN_OK: &[u8] =
        include_bytes!("../../programs/bpf_loader/test_elfs/out/sbpfv3_return_ok.so");
    const RETURN_ERR: &[u8] =
        include_bytes!("../../programs/bpf_loader/test_elfs/out/sbpfv3_return_err.so");

    fn store_program_with_bank_slot(bank: &Bank, program_id: &Pubkey, elf: &[u8]) {
        let [(_, program), (programdata_address, mut programdata)] =
            bpf_loader_upgradeable_program_accounts(
                program_id,
                elf,
                &Pubkey::default(),
                &Rent::default(),
            );
        // `bpf_loader_upgradeable_program_accounts` deploys at slot 0.
        programdata
            .set_state(&UpgradeableLoaderState::ProgramData {
                slot: bank.slot(),
                upgrade_authority_address: Some(Pubkey::default()),
            })
            .unwrap();
        bank.store_account(program_id, &AccountSharedData::from(program));
        bank.store_account(&programdata_address, &AccountSharedData::from(programdata));
    }

    fn invoke_transaction(bank: &Bank, payer: &Keypair, program_id: &Pubkey) -> Transaction {
        Transaction::new_signed_with_payer(
            &[create_invoke_instruction(payer.pubkey(), *program_id, &())],
            Some(&payer.pubkey()),
            &[payer],
            bank.last_blockhash(),
        )
    }

    fn load_and_execute(bank: &Bank, transaction: Transaction) -> Result<(), TransactionError> {
        let batch = bank.prepare_batch_for_tests(vec![transaction]);
        let mut output = bank.load_and_execute_transactions(
            &batch,
            bank.max_processing_age(),
            &mut ExecuteTimings::default(),
            &mut TransactionErrorMetrics::default(),
            TransactionProcessingConfig {
                limit_to_load_programs: true,
                recording_config: ExecutionRecordingConfig::new_single_setting(false),
                ..TransactionProcessingConfig::default()
            },
        );
        assert_eq!(output.processing_results.len(), 1);
        output
            .processing_results
            .pop()
            .unwrap()
            .and_then(|processed| processed.status())
    }

    #[test_case(false; "execute")]
    #[test_case(true; "simulate")]
    fn test_set_bank_forks_root_allows_execution_on_gap_bank(simulate: bool) {
        // Fork graph created for the test
        //
        //     0 - 1 - 2 - 3 - 4 - 5
        //         ^^^^^^^^^   ^   ^
        //         |           |   tip
        //         |           new root
        //         non-orphans, to be rooted
        //         deployment in (1), loaded in (2), upgraded in (3)
        //
        // Since these non-orphan banks are never quiesced, they can actually
        // execute transactions. They just can't commit, since they're frozen.
        //
        // This is fine for program cache purposes. Since:
        // - These invocations load entries into the global cache, even without
        //   committing, but only for their own fork
        // - The state they load is about to be rooted
        //
        // A gap bank can even reload a deployment that the root has since
        // pruned. The tip never uses it, since it sees the upgrade.
        //
        const DEPLOYMENT_SLOT: Slot = 1;
        const LOAD_SLOT: Slot = 2;
        const UPGRADE_SLOT: Slot = 3;
        const NEW_ROOT: Slot = 4;
        const TIP_SLOT: Slot = 5;

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair: payer,
            ..
        } = create_genesis_config(1_000_000_000_000);
        let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let env = current_env(&bank);
        goto_end_of_slot(bank.clone());

        // Program is deployed in a not-yet-rooted gap slot.
        let program = Pubkey::new_unique();
        let deployment_bank = Bank::new_from_parent_with_bank_forks(
            bank_forks.as_ref(),
            bank,
            SlotLeader::default(),
            DEPLOYMENT_SLOT,
        );
        store_program_with_bank_slot(&deployment_bank, &program, RETURN_OK);
        let gap_bank = Bank::new_from_parent_with_bank_forks(
            bank_forks.as_ref(),
            deployment_bank,
            SlotLeader::default(),
            LOAD_SLOT,
        );

        // And upgraded in the last gap slot.
        let upgrade_bank = Bank::new_from_parent_with_bank_forks(
            bank_forks.as_ref(),
            gap_bank.clone(),
            SlotLeader::default(),
            UPGRADE_SLOT,
        );
        store_program_with_bank_slot(&upgrade_bank, &program, RETURN_ERR);
        let root_bank = Bank::new_from_parent_with_bank_forks(
            bank_forks.as_ref(),
            upgrade_bank,
            SlotLeader::default(),
            NEW_ROOT,
        );
        let tip = Bank::new_from_parent_with_bank_forks(
            bank_forks.as_ref(),
            root_bank,
            SlotLeader::default(),
            TIP_SLOT,
        );
        // Simulation requires a frozen bank.
        if simulate {
            tip.freeze();
        }

        let transaction = invoke_transaction(&gap_bank, &payer, &program);
        let process = |bank: &Bank| {
            if simulate {
                let transaction =
                    RuntimeTransaction::from_transaction_for_tests(transaction.clone());
                bank.simulate_transaction(&transaction, false).result
            } else {
                load_and_execute(bank, transaction.clone())
            }
        };
        // Before rooting, each bank runs the version on its own fork.
        assert_eq!(
            process(&gap_bank),
            Ok(()),
            "the gap bank should execute before rooting",
        );
        let upgraded_result = process(&tip);
        assert!(
            matches!(
                upgraded_result,
                Err(TransactionError::InstructionError(0, _))
            ),
            "the tip should run the upgrade, got {upgraded_result:?}",
        );
        assert_cache_contents(
            &tip,
            &program,
            &[
                ExpectedEntry {
                    entry_type: EntryType::Loaded,
                    deployment_slot: DEPLOYMENT_SLOT,
                    environment: &env,
                },
                ExpectedEntry {
                    entry_type: EntryType::Loaded,
                    deployment_slot: UPGRADE_SLOT,
                    environment: &env,
                },
            ],
        );

        // Do rooting.
        let (drop_bank_sender, _drop_bank_receiver) = unbounded();
        set_bank_forks_root(
            &Pubkey::new_unique(),
            NEW_ROOT,
            &bank_forks,
            /* snapshot_controller */ None,
            /* highest_super_majority_root */ None,
            &drop_bank_sender,
            |_| {},
        );

        // The gap banks left the fork tree.
        assert_eq!(bank_forks.read().unwrap().root(), NEW_ROOT);
        for slot in [DEPLOYMENT_SLOT, LOAD_SLOT, UPGRADE_SLOT] {
            assert!(bank_forks.read().unwrap().get(slot).is_none());
        }

        // Rooting pruned the superseded deployment.
        assert_cache_contents(
            &tip,
            &program,
            &[ExpectedEntry {
                entry_type: EntryType::Loaded,
                deployment_slot: UPGRADE_SLOT,
                environment: &env,
            }],
        );

        // But a holder of the gap bank can still invoke, reloading it.
        assert!(gap_bank.try_enter_transaction_execution().is_some());
        assert_eq!(
            process(&gap_bank),
            Ok(()),
            "the gap bank should still execute after rooting",
        );
        assert_cache_contents(
            &tip,
            &program,
            &[
                ExpectedEntry {
                    entry_type: EntryType::Loaded,
                    deployment_slot: DEPLOYMENT_SLOT,
                    environment: &env,
                },
                ExpectedEntry {
                    entry_type: EntryType::Loaded,
                    deployment_slot: UPGRADE_SLOT,
                    environment: &env,
                },
            ],
        );

        // The tip is unaffected by the reloaded entry.
        assert_eq!(
            process(&tip),
            upgraded_result,
            "the tip should still run the upgrade",
        );
    }

    #[test]
    fn test_set_bank_forks_root_quiesces_and_prunes_orphan() {
        // Fork graph created for the test
        //
        //     0 - 1 - 2 - 3
        //          \
        //           4 - 5
        //           ^^^^^
        //           orphans, quiesced and pruned
        //           deployment in (4), loaded in (5)
        //
        // Assert that:
        // - Quiesced orphan banks cannot execute
        // - The orphans' deployment is pruned from the cache
        //
        const FORK_SLOT: Slot = 1;
        const ROOTED_LOAD_SLOT: Slot = 2;
        const NEW_ROOT: Slot = 3;
        const ORPHAN_SLOT: Slot = 4;
        const ORPHAN_LOAD_SLOT: Slot = 5;

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair: payer,
            ..
        } = create_genesis_config(1_000_000_000_000);
        let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let env = current_env(&bank);
        goto_end_of_slot(bank.clone());

        // Initial deployment at the shared parent.
        let fork_bank = Bank::new_from_parent_with_bank_forks(
            bank_forks.as_ref(),
            bank,
            SlotLeader::default(),
            FORK_SLOT,
        );
        let fork_program = Pubkey::new_unique();
        store_program_with_bank_slot(&fork_bank, &fork_program, RETURN_OK);

        // Load it on the chain that gets rooted.
        let rooted_load_bank = Bank::new_from_parent_with_bank_forks(
            bank_forks.as_ref(),
            fork_bank.clone(),
            SlotLeader::default(),
            ROOTED_LOAD_SLOT,
        );
        assert_eq!(
            rooted_load_bank.process_transaction(&invoke_transaction(
                &rooted_load_bank,
                &payer,
                &fork_program
            )),
            Ok(())
        );
        Bank::new_from_parent_with_bank_forks(
            bank_forks.as_ref(),
            rooted_load_bank,
            SlotLeader::default(),
            NEW_ROOT,
        );

        // Branch off the fork bank to create the orphans, deploying in the
        // first and loading in the second.
        let orphan = Bank::new_from_parent_with_bank_forks(
            bank_forks.as_ref(),
            fork_bank,
            SlotLeader::default(),
            ORPHAN_SLOT,
        );
        let orphan_program = Pubkey::new_unique();
        store_program_with_bank_slot(&orphan, &orphan_program, RETURN_OK);
        let orphan_load_bank = Bank::new_from_parent_with_bank_forks(
            bank_forks.as_ref(),
            orphan.clone(),
            SlotLeader::default(),
            ORPHAN_LOAD_SLOT,
        );
        assert_eq!(
            orphan_load_bank.process_transaction(&invoke_transaction(
                &orphan_load_bank,
                &payer,
                &orphan_program
            )),
            Ok(())
        );
        for (program, deployment_slot) in [(fork_program, FORK_SLOT), (orphan_program, ORPHAN_SLOT)]
        {
            assert_cache_contents(
                &orphan_load_bank,
                &program,
                &[ExpectedEntry {
                    entry_type: EntryType::Loaded,
                    deployment_slot,
                    environment: &env,
                }],
            );
        }

        let (drop_bank_sender, _drop_bank_receiver) = unbounded();
        set_bank_forks_root(
            &Pubkey::new_unique(),
            NEW_ROOT,
            &bank_forks,
            /* snapshot_controller */ None,
            /* highest_super_majority_root */ None,
            &drop_bank_sender,
            |_| {},
        );

        assert_eq!(bank_forks.read().unwrap().root(), NEW_ROOT);
        for slot in [ORPHAN_SLOT, ORPHAN_LOAD_SLOT] {
            assert!(bank_forks.read().unwrap().get(slot).is_none());
        }

        // The orphans' deployment went with them.
        assert_cache_contents(&orphan, &orphan_program, &[]);
        // The fork bank's deployment is now rooted. It survives.
        assert_cache_contents(
            &bank_forks.read().unwrap().root_bank(),
            &fork_program,
            &[ExpectedEntry {
                entry_type: EntryType::Loaded,
                deployment_slot: FORK_SLOT,
                environment: &env,
            }],
        );

        // And unlike the rooted chain's banks, the orphans' gates are closed for good.
        for bank in [&orphan, &orphan_load_bank] {
            assert!(
                bank.try_enter_transaction_execution().is_none(),
                "the orphan should have been quiesced",
            );
            assert_eq!(
                load_and_execute(bank, invoke_transaction(bank, &payer, &fork_program)),
                Err(TransactionError::CommitCancelled),
                "the orphan should be turned away after rooting",
            );
        }
    }
}
