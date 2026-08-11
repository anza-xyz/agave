use {
    crossbeam_channel::bounded,
    itertools::Itertools,
    log::*,
    solana_account::AccountSharedData,
    solana_accounts_db::accounts_scan::ScanError,
    solana_core::drop_bank_service::DropBankService,
    solana_leader_schedule::SlotLeader,
    solana_ledger::genesis_utils::create_genesis_config,
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::Bank,
        bank_forks::BankForks,
        genesis_utils::GenesisConfigInfo,
        installed_scheduler_pool::{DropBankRequest, SchedulingContext},
    },
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_svm_timings::ExecuteTimings,
    solana_system_transaction as system_transaction,
    solana_transaction_error::TransactionResult as Result,
    solana_unified_scheduler_logic::Task,
    solana_unified_scheduler_pool::{
        DefaultTaskHandler, HandlerContext, PooledScheduler, SchedulerPool, TaskHandler,
    },
    std::{
        sync::{Arc, Mutex},
        time::Duration,
    },
};

#[test]
fn test_drop_bank_service_flush_waits_for_unrooted_retirement() {
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
    let pool_raw = SchedulerPool::<PooledScheduler<StallingHandler>, _>::new_for_verification(
        None, None, None, None, None,
    );
    let pool = pool_raw.clone();
    bank_forks.write().unwrap().install_scheduler_pool(pool);
    let genesis = 0;
    let genesis_bank = bank_forks.read().unwrap().get(genesis).unwrap();
    genesis_bank.set_fork_graph_in_program_cache(Arc::downgrade(&bank_forks));

    // Create a divergent parent and an unfrozen child above the eventual root. These slots can be
    // replayed after they are pruned, so DropBankService must retire their exact account
    // generations. Creating the child freezes its parent while leaving the child unfrozen.
    let pruned_parent = 2;
    let pruned_parent_bank =
        Bank::new_from_parent(genesis_bank.clone(), SlotLeader::default(), pruned_parent);
    let inherited_account = Pubkey::new_unique();
    pruned_parent_bank.store_account(
        &inherited_account,
        &AccountSharedData::new(24, 0, &Pubkey::default()),
    );
    let pruned_parent_bank = bank_forks.write().unwrap().insert(pruned_parent_bank);
    let pruned_parent_bank_arc = pruned_parent_bank.clone_without_scheduler();
    drop(pruned_parent_bank);

    let pruned = 4;
    let pruned_bank = Bank::new_from_parent(
        pruned_parent_bank_arc.clone(),
        SlotLeader::default(),
        pruned,
    );
    let stale_account = Pubkey::new_unique();
    pruned_bank.store_account(
        &stale_account,
        &AccountSharedData::new(42, 0, &Pubkey::default()),
    );
    let pruned_bank = bank_forks
        .write()
        .unwrap()
        .insert_for_block_production(pruned_bank);
    let pruned_bank_arc = pruned_bank.clone_without_scheduler();
    assert_eq!(pruned_bank_arc.get_balance(&stale_account), 42);
    assert_eq!(pruned_bank_arc.get_balance(&inherited_account), 24);
    assert_eq!(pruned_bank_arc.scan_all_accounts(|_| {}), Ok(()));

    // An unfrozen bank not published for transaction production cannot receive BankingStage
    // commits, so it is safe to retire immediately.
    let remote = 5;
    let remote_bank = Bank::new_from_parent(
        pruned_parent_bank_arc.clone(),
        SlotLeader::new_unique(),
        remote,
    );
    let remote_account = Pubkey::new_unique();
    remote_bank.store_account(
        &remote_account,
        &AccountSharedData::new(12, 0, &Pubkey::default()),
    );
    let remote_signature = remote_bank
        .transfer(1, &mint_keypair, &Pubkey::new_unique())
        .unwrap();
    let remote_bank = bank_forks.write().unwrap().insert(remote_bank);
    let remote_bank_arc = remote_bank.clone_without_scheduler();
    assert!(
        remote_bank_arc
            .get_signature_status(&remote_signature)
            .is_some()
    );
    drop(remote_bank);

    // Create new root bank
    let root = 1;
    let root_bank = Bank::new_from_parent(genesis_bank.clone(), SlotLeader::default(), root);
    root_bank.freeze();
    bank_forks.write().unwrap().insert(root_bank);

    let tx = RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
        &mint_keypair,
        &solana_pubkey::new_rand(),
        2,
        genesis_config.hash(),
    ));

    // Block transaction execution so retirement cannot complete until the test releases it.
    let lock_to_stall = LOCK_TO_STALL.lock().unwrap();
    pruned_bank
        .schedule_transaction_executions([(tx, 0)].into_iter())
        .unwrap();
    drop(pruned_bank);
    assert_eq!(pool_raw.pooled_scheduler_count(), 0);

    // Call BankForks::set_root directly: root_utils intentionally waits for schedulers before
    // pruning, while this test needs the bank to reach DropBankService with work still in flight.
    let pruned_banks = bank_forks.write().unwrap().set_root(root, None, None);
    assert_eq!(
        pruned_banks
            .iter()
            .map(|b| b.slot())
            .sorted()
            .collect::<Vec<_>>(),
        vec![genesis, pruned_parent, pruned, remote]
    );

    let (drop_bank_sender, drop_bank_receiver) = bounded(1024);
    let drop_bank_service = DropBankService::new(drop_bank_receiver);
    drop_bank_sender
        .send(DropBankRequest::DropBanks {
            banks: pruned_banks,
            new_root: root,
        })
        .unwrap();

    // The frozen parent remains protected while its producer child is live, while the unrelated
    // non-producer bank retires immediately. Barrier reports the matching pending dependency group.
    let (barrier_ack_sender, barrier_ack_receiver) = bounded(1);
    drop_bank_sender
        .send(DropBankRequest::Barrier {
            slots: vec![pruned_parent],
            ack_sender: barrier_ack_sender,
        })
        .unwrap();
    assert!(
        barrier_ack_receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
    );
    assert_eq!(remote_bank_arc.get_balance(&remote_account), 0);
    assert!(
        remote_bank_arc
            .get_signature_status(&remote_signature)
            .is_none()
    );
    assert_eq!(
        remote_bank_arc.scan_all_accounts(|_| {}),
        Err(ScanError::SlotRemoved {
            slot: remote,
            bank_id: remote_bank_arc.bank_id(),
        })
    );
    assert_eq!(pruned_bank_arc.get_balance(&stale_account), 42);
    assert_eq!(pruned_bank_arc.get_balance(&inherited_account), 24);
    assert_eq!(pruned_bank_arc.scan_all_accounts(|_| {}), Ok(()));
    assert_eq!(pruned_parent_bank_arc.scan_all_accounts(|_| {}), Ok(()));

    let (unrelated_ack_sender, unrelated_ack_receiver) = bounded(1);
    drop_bank_sender
        .send(DropBankRequest::Barrier {
            slots: vec![root],
            ack_sender: unrelated_ack_sender,
        })
        .unwrap();
    assert!(
        !unrelated_ack_receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
    );

    let (ack_sender, ack_receiver) = bounded(1);
    drop_bank_sender
        .send(DropBankRequest::Flush {
            // Targeting an ancestor must also retire its pending descendants.
            slots: vec![pruned_parent],
            ack_sender,
        })
        .unwrap();

    // Flush is ordered behind retirement and cannot acknowledge while slot 4's scheduler is
    // blocked. Account state and scan eligibility must remain intact until execution quiesces.
    assert!(
        ack_receiver
            .recv_timeout(Duration::from_millis(50))
            .is_err()
    );
    assert_eq!(pruned_bank_arc.get_balance(&stale_account), 42);
    assert_eq!(pruned_bank_arc.scan_all_accounts(|_| {}), Ok(()));

    // Model a BankingStage record that has reserved this bank but has not completed its account
    // commit. Retirement must fence this legacy path independently of unified-scheduler work.
    let inflight_commit = pruned_bank_arc.freeze_lock();
    drop(lock_to_stall);
    assert!(
        ack_receiver
            .recv_timeout(Duration::from_millis(50))
            .is_err()
    );
    assert_eq!(pruned_bank_arc.get_balance(&stale_account), 42);

    drop(inflight_commit);
    ack_receiver.recv_timeout(Duration::from_secs(5)).unwrap();

    // The acknowledgement fences both account retirement and scan invalidation for the exact
    // unrooted bank generation. The removed genesis Bank is at or below the new root and must not
    // be tombstoned.
    assert_eq!(pruned_bank_arc.get_balance(&stale_account), 0);
    assert_eq!(pruned_bank_arc.get_balance(&inherited_account), 0);
    assert_eq!(
        pruned_bank_arc.scan_all_accounts(|_| {}),
        Err(ScanError::SlotRemoved {
            slot: pruned,
            bank_id: pruned_bank_arc.bank_id(),
        })
    );
    assert_eq!(
        pruned_parent_bank_arc.scan_all_accounts(|_| {}),
        Err(ScanError::SlotRemoved {
            slot: pruned_parent,
            bank_id: pruned_parent_bank_arc.bank_id(),
        })
    );
    assert_eq!(genesis_bank.scan_all_accounts(|_| {}), Ok(()));

    drop(drop_bank_sender);
    drop_bank_service.join().unwrap();

    // The flush acknowledgement is also ordered after the pruned bank releases its scheduler.
    assert_eq!(pool_raw.pooled_scheduler_count(), 3);
}
