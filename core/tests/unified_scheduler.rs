use {
    agave_banking_stage_ingress_types::BankingPacketBatch,
    assert_matches::assert_matches,
    crossbeam_channel::unbounded,
    itertools::Itertools,
    log::*,
    solana_core::{
        banking_stage::{unified_scheduler::ensure_banking_stage_setup, BankingStage},
        banking_trace::BankingTracer,
        consensus::{
            heaviest_subtree_fork_choice::HeaviestSubtreeForkChoice,
            progress_map::{ForkProgress, ProgressMap},
        },
        drop_bank_service::DropBankService,
        repair::cluster_slot_state_verifier::{
            DuplicateConfirmedSlots, DuplicateSlotsTracker, EpochSlotsFrozenSlots,
        },
        replay_stage::{ReplayStage, TowerBFTStructures},
        unfrozen_gossip_verified_vote_hashes::UnfrozenGossipVerifiedVoteHashes,
    },
    solana_entry::entry::Entry,
    solana_hash::Hash,
    solana_ledger::{
        blockstore::Blockstore, create_new_tmp_ledger_auto_delete,
        genesis_utils::create_genesis_config, leader_schedule_cache::LeaderScheduleCache,
    },
    solana_perf::packet::to_packet_batches,
    solana_poh::poh_recorder::create_test_recorder,
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::Bank, bank_forks::BankForks, genesis_utils::GenesisConfigInfo,
        installed_scheduler_pool::SchedulingContext,
        prioritization_fee_cache::PrioritizationFeeCache,
    },
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_svm_timings::ExecuteTimings,
    solana_system_transaction as system_transaction,
    solana_transaction_error::TransactionResult as Result,
    solana_unified_scheduler_logic::{SchedulingMode, Task},
    solana_unified_scheduler_pool::{
        DefaultSchedulerPool, DefaultTaskHandler, HandlerContext, PooledScheduler, SchedulerPool,
        TaskHandler,
    },
    std::{
        collections::HashMap,
        sync::{atomic::Ordering, Arc, Mutex},
        thread::sleep,
        time::Duration,
    },
};

#[test]
fn test_scheduler_waited_by_drop_bank_service() {
    agave_logger::setup();

    static LOCK_TO_STALL: Mutex<()> = Mutex::new(());

    #[derive(Debug)]
    struct StallingHandler;
    impl TaskHandler for StallingHandler {
        fn handle(
            result: &mut Result<()>,
            timings: &mut ExecuteTimings,
            scheduling_context: &SchedulingContext,
            task: &Task,
            handler_context: &HandlerContext,
        ) {
            info!("Stalling at StallingHandler::handle()...");
            *LOCK_TO_STALL.lock().unwrap();
            // Wait a bit for the replay stage to prune banks
            std::thread::sleep(std::time::Duration::from_secs(3));
            info!("Now entering into DefaultTaskHandler::handle()...");

            DefaultTaskHandler::handle(result, timings, scheduling_context, task, handler_context);
        }
    }

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(10_000);

    // Setup bankforks with unified scheduler enabled
    let genesis_bank = Bank::new_for_tests(&genesis_config);
    let bank_forks = BankForks::new_rw_arc(genesis_bank);
    let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
    let pool_raw = SchedulerPool::<PooledScheduler<StallingHandler>, _>::new(
        None,
        None,
        None,
        None,
        ignored_prioritization_fee_cache,
    );
    let pool = pool_raw.clone();
    bank_forks.write().unwrap().install_scheduler_pool(pool);
    let genesis = 0;
    let genesis_bank = &bank_forks.read().unwrap().get(genesis).unwrap();
    genesis_bank.set_fork_graph_in_program_cache(Arc::downgrade(&bank_forks));

    // Create bank, which is pruned later
    let pruned = 2;
    let pruned_bank = Bank::new_from_parent(genesis_bank.clone(), &Pubkey::default(), pruned);
    let pruned_bank = bank_forks.write().unwrap().insert(pruned_bank);

    // Create new root bank
    let root = 3;
    let root_bank = Bank::new_from_parent(genesis_bank.clone(), &Pubkey::default(), root);
    root_bank.freeze();
    let root_hash = root_bank.hash();
    bank_forks.write().unwrap().insert(root_bank);

    let tx = RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
        &mint_keypair,
        &solana_pubkey::new_rand(),
        2,
        genesis_config.hash(),
    ));

    // Delay transaction execution to ensure transaction execution happens after termination has
    // been started
    let lock_to_stall = LOCK_TO_STALL.lock().unwrap();
    pruned_bank
        .schedule_transaction_executions([(tx, 0)].into_iter())
        .unwrap();
    drop(pruned_bank);
    assert_eq!(pool_raw.pooled_scheduler_count(), 0);
    drop(lock_to_stall);

    // Create 2 channels to check actual pruned banks
    let (drop_bank_sender1, drop_bank_receiver1) = unbounded();
    let (drop_bank_sender2, drop_bank_receiver2) = unbounded();
    let drop_bank_service = DropBankService::new(drop_bank_receiver2);

    info!("calling handle_new_root()...");
    // Mostly copied from: test_handle_new_root()
    {
        let heaviest_subtree_fork_choice = HeaviestSubtreeForkChoice::new((root, root_hash));

        let mut progress = ProgressMap::default();
        for i in genesis..=root {
            progress.insert(i, ForkProgress::new(Hash::default(), None, None, 0, 0));
        }

        let duplicate_slots_tracker: DuplicateSlotsTracker =
            vec![root - 1, root, root + 1].into_iter().collect();
        let duplicate_confirmed_slots: DuplicateConfirmedSlots = vec![root - 1, root, root + 1]
            .into_iter()
            .map(|s| (s, Hash::default()))
            .collect();
        let unfrozen_gossip_verified_vote_hashes: UnfrozenGossipVerifiedVoteHashes =
            UnfrozenGossipVerifiedVoteHashes {
                votes_per_slot: vec![root - 1, root, root + 1]
                    .into_iter()
                    .map(|s| (s, HashMap::new()))
                    .collect(),
            };
        let epoch_slots_frozen_slots: EpochSlotsFrozenSlots = vec![root - 1, root, root + 1]
            .into_iter()
            .map(|slot| (slot, Hash::default()))
            .collect();
        let mut tbft_structs = TowerBFTStructures {
            heaviest_subtree_fork_choice,
            duplicate_slots_tracker,
            duplicate_confirmed_slots,
            unfrozen_gossip_verified_vote_hashes,
            epoch_slots_frozen_slots,
        };
        ReplayStage::handle_new_root(
            root,
            &bank_forks,
            &mut progress,
            None, // snapshot_controller
            None,
            &mut true,
            &mut Vec::new(),
            &drop_bank_sender1,
            &mut tbft_structs,
        );
    }

    // Receive pruned banks from the above handle_new_root
    let pruned_banks = drop_bank_receiver1.recv().unwrap();
    assert_eq!(
        pruned_banks
            .iter()
            .map(|b| b.slot())
            .sorted()
            .collect::<Vec<_>>(),
        vec![genesis, pruned]
    );
    info!("sending pruned banks to DropBankService...");
    drop_bank_sender2.send(pruned_banks).unwrap();

    info!("joining the drop bank service...");
    drop((
        (drop_bank_sender1, drop_bank_receiver1),
        (drop_bank_sender2,),
    ));
    drop_bank_service.join().unwrap();
    info!("finally joined the drop bank service!");

    // the scheduler used by the pruned_bank have been returned now.
    assert_eq!(pool_raw.pooled_scheduler_count(), 1);
}

#[test]
fn test_scheduler_producing_blocks() {
    agave_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(10_000);
    let (ledger_path, _blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);
    let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());

    // Setup bank_forks with block-producing unified scheduler enabled
    let genesis_bank = Bank::new_for_tests(&genesis_config);
    let bank_forks = BankForks::new_rw_arc(genesis_bank);
    let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
    let genesis_bank = bank_forks.read().unwrap().working_bank_with_scheduler();
    genesis_bank.set_fork_graph_in_program_cache(Arc::downgrade(&bank_forks));
    let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&genesis_bank));
    let (
        exit,
        poh_recorder,
        mut poh_controller,
        transaction_recorder,
        poh_service,
        signal_receiver,
    ) = create_test_recorder(
        genesis_bank.clone(),
        blockstore.clone(),
        None,
        Some(leader_schedule_cache),
    );
    let pool = DefaultSchedulerPool::new(None, None, None, None, ignored_prioritization_fee_cache);
    let channels = {
        let banking_tracer = BankingTracer::new_disabled();
        banking_tracer.create_channels(true)
    };
    ensure_banking_stage_setup(
        &pool,
        &bank_forks,
        &channels,
        &poh_recorder,
        transaction_recorder,
        BankingStage::default_num_workers(),
    );
    bank_forks.write().unwrap().install_scheduler_pool(pool);

    // Wait until genesis_bank reaches its tick height...
    while poh_recorder.read().unwrap().bank().is_some() {
        sleep(Duration::from_millis(100));
    }

    // Create test tx
    let tx = system_transaction::transfer(
        &mint_keypair,
        &solana_pubkey::new_rand(),
        1,
        genesis_config.hash(),
    );
    let banking_packet_batch = BankingPacketBatch::new(to_packet_batches(&vec![tx.clone(); 1], 1));
    let tx = RuntimeTransaction::from_transaction_for_tests(tx);

    // Crate tpu_bank
    let tpu_bank = Bank::new_from_parent(genesis_bank.clone(), &Pubkey::default(), 2);
    let tpu_bank = bank_forks
        .write()
        .unwrap()
        .insert_with_scheduling_mode(SchedulingMode::BlockProduction, tpu_bank);
    poh_controller
        .set_bank_sync(tpu_bank.clone_with_scheduler())
        .unwrap();
    tpu_bank.unpause_new_block_production_scheduler();
    let tpu_bank = bank_forks.read().unwrap().working_bank_with_scheduler();
    assert_eq!(tpu_bank.transaction_count(), 0);

    // Now, send transaction
    channels
        .unified_sender()
        .send(banking_packet_batch)
        .unwrap();

    // Wait until tpu_bank reaches its tick height...
    while poh_recorder.read().unwrap().bank().is_some() {
        sleep(Duration::from_millis(100));
    }
    assert_matches!(tpu_bank.wait_for_completed_scheduler(), Some((Ok(()), _)));

    // Verify transactions are committed and poh-recorded
    assert_eq!(tpu_bank.transaction_count(), 1);
    assert_matches!(
        signal_receiver.into_iter().find(|(_, (entry, _))| !entry.is_tick()),
        Some((_, (Entry {transactions, ..}, _))) if transactions == [tx.to_versioned_transaction()]
    );

    // Stop things.
    exit.store(true, Ordering::Relaxed);
    poh_service.join().unwrap();
}
