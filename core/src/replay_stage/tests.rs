use {
    super::*,
    crate::consensus::progress_map::DeadSlotReason,
    assert_matches::assert_matches,
    crossbeam_channel::bounded,
    solana_accounts_db::accounts_db::{ACCOUNTS_DB_CONFIG_FOR_TESTING, AccountsDbConfig},
    solana_entry::{
        block_component::{
            BlockComponent, BlockFooterV1, BlockHeaderV1, UpdateParentV1, VersionedBlockMarker,
            VersionedUpdateParent,
        },
        entry::{self, Entry},
    },
    solana_epoch_schedule::EpochSchedule,
    solana_hash::Hash,
    solana_instruction_error::InstructionError,
    solana_keypair::Keypair,
    solana_ledger::{
        block_error::BlockError,
        blockstore::{Blockstore, BlockstoreError, UpdateParentSignal},
        blockstore_meta::BlockLocation,
        create_new_tmp_ledger,
        entry_notifier_service::EntryNotification,
        genesis_utils::{create_genesis_config, create_genesis_config_with_leader},
        get_tmp_ledger_path,
        shred::{ProcessShredsStats, Shred, Shredder},
    },
    solana_rpc::{
        rpc::{create_test_transaction_entries, populate_blockstore_for_tests},
        slot_status_notifier::SlotStatusNotifierInterface,
    },
    solana_runtime::{
        bank::{BankId, BankTestConfig},
        genesis_utils::GenesisConfigInfo,
        transaction_execution::{
            TransactionHistoryPurgeInput, TransactionHistoryPurgeSource, TransactionStatusMessage,
        },
    },
    solana_sha256_hasher::hash,
    solana_signature::Signature,
    solana_signer::Signer,
    solana_system_transaction as system_transaction,
    solana_transaction::Transaction,
    solana_transaction_error::TransactionError,
    solana_transaction_status::VersionedTransactionWithStatusMeta,
    solana_unified_scheduler_pool::DefaultSchedulerPool,
    std::{
        collections::HashSet,
        sync::{Arc, Mutex, atomic::AtomicU64},
        time::Instant,
    },
    test_case::test_case,
    trees::{Tree, TreeWalk, tr},
};

/// Tower-free bank/ledger fixture for ReplayStage's Alpenglow tests.
struct AlpenglowReplayComponents {
    blockstore: Arc<Blockstore>,
    bank_forks: Arc<RwLock<BankForks>>,
    progress: ProgressMap,
    mint_keypair: Keypair,
    my_pubkey: Pubkey,
    leader_schedule_cache: Arc<LeaderScheduleCache>,
}

fn alpenglow_replay_components(forks: Tree<Slot>) -> AlpenglowReplayComponents {
    let ledger_path = get_tmp_ledger_path!();
    let my_pubkey = Pubkey::new_unique();
    let mut genesis = create_genesis_config_with_leader(1_000_000_000, &my_pubkey, 10_000);
    genesis.genesis_config.epoch_schedule = EpochSchedule::without_warmup();
    let bank0 = Bank::new_with_paths_for_tests(
        &genesis.genesis_config,
        Some(BankTestConfig {
            accounts_db_config: AccountsDbConfig {
                bank_hash_details_dir: ledger_path.clone(),
                ..ACCOUNTS_DB_CONFIG_FOR_TESTING
            },
        }),
        vec![],
        None,
    );
    bank0.set_block_id(Some(BlockId::default()));
    bank0.fill_bank_with_ticks_for_tests();
    bank0.freeze();
    let bank_forks = BankForks::new_rw_arc(bank0);

    assert_eq!(*forks.root().data(), 0);
    let mut walk = TreeWalk::from(forks.clone());
    while let Some(visit) = walk.get() {
        let slot = *visit.node().data();
        if bank_forks.read().unwrap().get(slot).is_some() {
            walk.forward();
            continue;
        }
        let parent_slot = *walk.get_parent().unwrap().data();
        let parent_bank = bank_forks.read().unwrap().get(parent_slot).unwrap();
        let bank = Bank::new_from_parent(parent_bank, SlotLeader::default(), slot);
        bank.fill_bank_with_ticks_for_tests();
        bank.set_block_id(Some(BlockId::new_unique()));
        bank.freeze();
        bank_forks.write().unwrap().insert(bank);
        walk.forward();
    }

    let root_bank = bank_forks.read().unwrap().root_bank();
    let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&root_bank));
    let progress = ReplayStage::initialize_progress(&bank_forks);
    let blockstore = Arc::new(Blockstore::open(&ledger_path).unwrap());
    blockstore.add_tree(forks, false, true, 2, Hash::default());

    AlpenglowReplayComponents {
        blockstore,
        bank_forks,
        progress,
        mint_keypair: genesis.mint_keypair,
        my_pubkey,
        leader_schedule_cache,
    }
}

impl ProcessActiveBanksContext {
    fn new_for_tests(bank_forks: Arc<RwLock<BankForks>>, blockstore: Arc<Blockstore>) -> Self {
        let (cost_update_sender, _) = bounded(1024);
        let (votor_event_sender, _) = bounded(1024);
        let migration_status = bank_forks.read().unwrap().migration_status();
        Self {
            bank_forks,
            blockstore,
            transaction_status_sender: None,
            entry_notification_sender: None,
            bank_notification_sender: None,
            rpc_subscriptions: None,
            slot_status_notifier: None,
            cost_update_sender,
            block_metadata_notifier: None,
            votor_event_sender,
            replay_mode: ForkReplayMode::Serial,
            replay_verification_worker_pool: ReplayVerificationWorkerPool::new(NonZeroUsize::MIN),
            migration_status,
        }
    }
}

fn transaction_history_purge_responder() -> (
    TransactionStatusSender,
    std::thread::JoinHandle<(Slot, TransactionHistoryPurgeSource)>,
) {
    let (sender, receiver) = bounded(1);
    let response_thread = std::thread::spawn(move || {
        let TransactionStatusMessage::PurgeTransactionHistory {
            slot,
            source,
            purge_input,
            done_sender,
            ..
        } = receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap()
        else {
            panic!("expected transaction-history purge request");
        };
        assert!(matches!(
            purge_input,
            TransactionHistoryPurgeInput::ReplayStage
        ));
        assert!(done_sender.is_none());
        (slot, source)
    });
    (
        TransactionStatusSender {
            sender,
            dependency_tracker: None,
        },
        response_thread,
    )
}

fn block_marker_shreds(
    slot: Slot,
    shred_parent_slot: Slot,
    marker: VersionedBlockMarker,
    shred_index: u32,
) -> Vec<Shred> {
    block_marker_shreds_with_last(slot, shred_parent_slot, marker, shred_index, false)
}

fn block_marker_shreds_with_last(
    slot: Slot,
    shred_parent_slot: Slot,
    marker: VersionedBlockMarker,
    shred_index: u32,
    is_last_in_slot: bool,
) -> Vec<Shred> {
    let component = BlockComponent::new_block_marker(marker);
    Shredder::new(slot, shred_parent_slot, 0, 0)
        .unwrap()
        .make_merkle_shreds_from_component(
            &Keypair::new(),
            &component,
            is_last_in_slot,
            Hash::new_unique(),
            shred_index,
            shred_index,
            &mut ProcessShredsStats::default(),
        )
}

fn insert_update_parent_slot(
    blockstore: &Blockstore,
    slot: Slot,
    block_header_parent_slot: Slot,
    update_parent_slot: Slot,
    update_parent_block_id: BlockId,
    replay_fec_set_index: u32,
) {
    let header = VersionedBlockMarker::from_block_header(BlockHeaderV1 {
        parent_slot: block_header_parent_slot,
        parent_block_id: BlockId::new_unique(),
    });
    let update_parent = VersionedBlockMarker::from_update_parent(UpdateParentV1 {
        new_parent_slot: update_parent_slot,
        new_parent_block_id: update_parent_block_id,
    });
    let mut shreds = block_marker_shreds(slot, block_header_parent_slot, header, 0);
    shreds.extend(block_marker_shreds(
        slot,
        block_header_parent_slot,
        update_parent,
        replay_fec_set_index,
    ));
    blockstore.insert_shreds(shreds, true).unwrap();
}

fn dead_slot_context_for_tests(blockstore: Arc<Blockstore>) -> DeadSlotContext {
    DeadSlotContext {
        notifications: DeadSlotNotifications {
            blockstore,
            rpc_subscriptions: None,
            slot_status_notifier: None,
        },
    }
}

struct SlotStatusNotifierForTest {
    dead_slots: Arc<Mutex<HashSet<Slot>>>,
}

impl SlotStatusNotifierInterface for SlotStatusNotifierForTest {
    fn notify_slot_confirmed(&self, _slot: Slot, _parent: Option<Slot>, _bank_id: BankId) {}

    fn notify_slot_processed(&self, _slot: Slot, _parent: Option<Slot>, _bank_id: BankId) {}

    fn notify_slot_rooted(&self, _slot: Slot, _parent: Option<Slot>, _bank_id: BankId) {}

    fn notify_first_shred_received(&self, _slot: Slot) {}

    fn notify_completed(&self, _slot: Slot) {}

    fn notify_created_bank(&self, _slot: Slot, _parent: Slot, _bank_id: BankId) {}

    fn notify_slot_dead(&self, slot: Slot, _parent: Slot, _error: String) {
        self.dead_slots.lock().unwrap().insert(slot);
    }
}

fn alpenglow_entry_shreds(bank: &Bank, entries: &[Entry], is_last_in_slot: bool) -> Vec<Shred> {
    let slot = bank.slot();
    let parent_slot = bank.parent_slot();
    let header = VersionedBlockMarker::from_block_header(BlockHeaderV1 {
        parent_slot,
        parent_block_id: bank
            .parent()
            .and_then(|parent| parent.block_id())
            .unwrap_or_default(),
    });
    let mut shreds = block_marker_shreds(slot, parent_slot, header, 0)
        .into_iter()
        .filter(Shred::is_data)
        .collect::<Vec<_>>();
    let next_shred_index = shreds.last().unwrap().index() + 1;
    shreds.extend(
        Shredder::new(slot, parent_slot, 0, 0)
            .unwrap()
            .make_merkle_shreds_from_entries(
                &Keypair::new(),
                entries,
                is_last_in_slot,
                Hash::new_unique(),
                next_shred_index,
                next_shred_index,
                &mut ProcessShredsStats::default(),
            )
            .into_iter()
            .filter(Shred::is_data),
    );
    shreds
}

fn alpenglow_raw_shreds(bank: &Bank, bytes: &[u8], is_last_in_slot: bool) -> Vec<Shred> {
    let slot = bank.slot();
    let parent_slot = bank.parent_slot();
    let header = VersionedBlockMarker::from_block_header(BlockHeaderV1 {
        parent_slot,
        parent_block_id: bank
            .parent()
            .and_then(|parent| parent.block_id())
            .unwrap_or_default(),
    });
    let mut shreds = block_marker_shreds(slot, parent_slot, header, 0)
        .into_iter()
        .filter(Shred::is_data)
        .collect::<Vec<_>>();
    let next_shred_index = shreds.last().unwrap().index() + 1;
    shreds.extend(
        Shredder::new(slot, parent_slot, 0, 0)
            .unwrap()
            .make_shreds_from_data_slice(
                &Keypair::new(),
                bytes,
                is_last_in_slot,
                Hash::new_unique(),
                next_shred_index,
                next_shred_index,
                &mut ProcessShredsStats::default(),
            )
            .unwrap()
            .into_iter()
            .filter(Shred::is_data),
    );
    shreds
}

fn alpenglow_complete_slot_shreds(bank: &Bank, txs: Vec<Transaction>) -> Vec<Shred> {
    let slot = bank.slot();
    let parent_slot = bank.parent_slot();
    let parent_block_id = bank
        .parent()
        .and_then(|parent| parent.block_id())
        .unwrap_or_default();
    let header = VersionedBlockMarker::from_block_header(BlockHeaderV1 {
        parent_slot,
        parent_block_id,
    });
    let shredder = Shredder::new(slot, parent_slot, 0, 0).unwrap();
    let mut shreds = block_marker_shreds(slot, parent_slot, header, 0)
        .into_iter()
        .filter(Shred::is_data)
        .collect::<Vec<_>>();
    let mut next_shred_index = shreds.last().unwrap().index() + 1;

    let tx_entry = entry::next_entry(&bank.last_blockhash(), 1, txs);
    let transaction_shreds = shredder
        .make_merkle_shreds_from_entries(
            &Keypair::new(),
            std::slice::from_ref(&tx_entry),
            false,
            Hash::new_unique(),
            next_shred_index,
            next_shred_index,
            &mut ProcessShredsStats::default(),
        )
        .into_iter()
        .filter(Shred::is_data)
        .collect::<Vec<_>>();
    next_shred_index = transaction_shreds.last().unwrap().index() + 1;
    shreds.extend(transaction_shreds);

    let parent_time_nanos = bank
        .parent()
        .and_then(|parent| parent.get_nanosecond_clock())
        .unwrap_or_else(|| bank.clock().unix_timestamp.saturating_mul(1_000_000_000));
    let footer = VersionedBlockMarker::from_block_footer(BlockFooterV1 {
        bank_hash: Hash::new_unique(),
        block_producer_time_nanos: u64::try_from(parent_time_nanos.saturating_add(1)).unwrap(),
        block_user_agent: vec![],
        block_final_cert: None,
        skip_reward_cert: None,
        notar_reward_cert: None,
    });
    let footer_shreds = block_marker_shreds(slot, parent_slot, footer, next_shred_index)
        .into_iter()
        .filter(Shred::is_data)
        .collect::<Vec<_>>();
    next_shred_index = footer_shreds.last().unwrap().index() + 1;
    shreds.extend(footer_shreds);

    let alpentick = entry::next_entry(&tx_entry.hash, 1, vec![]);
    shreds.extend(
        shredder
            .make_merkle_shreds_from_entries(
                &Keypair::new(),
                &[alpentick],
                true,
                Hash::new_unique(),
                next_shred_index,
                next_shred_index,
                &mut ProcessShredsStats::default(),
            )
            .into_iter()
            .filter(Shred::is_data),
    );
    shreds
}

/// Replay malformed shreds and publish the resulting Alpenglow dead-slot transition.
fn check_dead_fork<F>(shred_to_insert: F) -> Result<(), BlockstoreProcessorError>
where
    F: Fn(&Keypair, Arc<Bank>) -> Vec<Shred>,
{
    let AlpenglowReplayComponents {
        blockstore,
        bank_forks,
        mut progress,
        mint_keypair,
        ..
    } = alpenglow_replay_components(tr(0));
    let bank0 = bank_forks.read().unwrap().get(0).unwrap();
    assert!(bank0.is_frozen());
    assert_eq!(bank0.tick_height(), bank0.max_tick_height());
    bank_forks
        .write()
        .unwrap()
        .install_scheduler_pool(DefaultSchedulerPool::new_for_verification(
            None, None, None, None, None,
        ));
    let bank1 = Bank::new_from_parent(bank0, SlotLeader::default(), 1);
    blockstore_processor::set_alpenglow_ticks(
        &bank1,
        &bank_forks.read().unwrap().migration_status(),
    );
    bank_forks.write().unwrap().insert(bank1);
    let bank1 = bank_forks.read().unwrap().get_with_scheduler(1).unwrap();
    let bank1_progress = progress
        .entry(bank1.slot())
        .or_insert_with(|| ForkProgress::new(bank1.last_blockhash(), 0, 0, None));
    let shreds = shred_to_insert(&mint_keypair, bank1.clone());
    blockstore.insert_shreds(shreds, false).unwrap();
    let slot_was_full = blockstore.meta(bank1.slot()).unwrap().unwrap().is_full();
    let (finalization_cert_sender, _finalization_cert_receiver) = bounded(1024);
    let context = ProcessActiveBanksContext::new_for_tests(bank_forks, blockstore.clone());
    let result = ReplayStage::replay_blockstore_into_bank(
        0,
        &context,
        &bank1,
        &bank1_progress.replay_stats,
        &bank1_progress.replay_progress,
        &finalization_cert_sender,
    )
    .and_then(|replay_tx_count| {
        let mut poh_verify_elapsed = 0;
        let mut tx_verify_elapsed = 0;
        let verify_result = bank1_progress
            .replay_progress
            .write()
            .unwrap()
            .wait_for_all_verification_results(&mut poh_verify_elapsed, &mut tx_verify_elapsed);
        {
            let mut stats = bank1_progress.replay_stats.write().unwrap();
            stats.poh_verify_elapsed += poh_verify_elapsed;
            stats.transaction_verify_elapsed += tx_verify_elapsed;
        }
        verify_result?;
        if let Some((result, _timings)) = bank1.wait_for_completed_scheduler() {
            result?;
        }
        Ok(replay_tx_count)
    });

    let dead_slots = Arc::new(Mutex::new(HashSet::new()));
    let slot_status_notifier: SlotStatusNotifier =
        Arc::new(RwLock::new(SlotStatusNotifierForTest {
            dead_slots: dead_slots.clone(),
        }));
    if let Err(err) = &result {
        mark_replay_dead_slot(
            &bank1,
            err,
            &mut progress,
            &DeadSlotContext {
                notifications: DeadSlotNotifications {
                    blockstore: blockstore.clone(),
                    rpc_subscriptions: None,
                    slot_status_notifier: Some(slot_status_notifier),
                },
            },
        );
    }

    let dead_reason = progress.get(&bank1.slot()).unwrap().dead_reason.as_ref();
    if slot_was_full {
        assert_eq!(dead_reason, Some(&DeadSlotReason::Hard));
        assert!(blockstore.is_dead(bank1.slot()));
        assert!(dead_slots.lock().unwrap().contains(&bank1.slot()));
    } else {
        assert_eq!(
            dead_reason,
            Some(&DeadSlotReason::ReplayFailureBeforeUpdateParent)
        );
        assert!(!blockstore.is_dead(bank1.slot()));
        assert!(!dead_slots.lock().unwrap().contains(&bank1.slot()));
    }
    bank1.quiesce_transaction_execution();
    let _ = bank1.wait_for_completed_scheduler();
    result.map(|_| ())
}

#[test]
fn test_far_future_optimistic_parent_requires_parent_window_ready() {
    let my_pubkey = Pubkey::new_unique();
    let mut genesis = create_genesis_config_with_leader(10_000, &my_pubkey, 1_000);
    genesis.genesis_config.epoch_schedule = EpochSchedule::without_warmup();
    let root_bank = Bank::new_for_tests(&genesis.genesis_config);
    root_bank.freeze();
    let bank_forks = BankForks::new_rw_arc(root_bank);
    let root_bank = bank_forks.read().unwrap().root_bank();
    let leader_schedule_cache = LeaderScheduleCache::new_from_bank(&root_bank);

    let parent_slot = 65_531;
    let parent_window_start = 65_528;
    let parent_leader = leader_schedule_cache
        .slot_leader_at(parent_slot, Some(&root_bank))
        .unwrap();
    let parent_bank =
        Bank::new_from_parent_with_bank_forks(&bank_forks, root_bank, parent_leader, parent_slot);
    let parent_block_id = BlockId::new_unique();
    parent_bank.set_block_id(Some(parent_block_id));
    parent_bank.freeze();

    let (sender, receiver) = bounded(1);
    for highest_parent_ready_slot in [4, parent_window_start - 1] {
        ReplayStage::maybe_notify_of_optimistic_parent(
            &parent_bank,
            &my_pubkey,
            &leader_schedule_cache,
            &sender,
            &receiver,
            highest_parent_ready_slot,
        );
        assert!(receiver.try_recv().is_err());
    }

    ReplayStage::maybe_notify_of_optimistic_parent(
        &parent_bank,
        &my_pubkey,
        &leader_schedule_cache,
        &sender,
        &receiver,
        parent_window_start,
    );
    let notification = receiver.try_recv().unwrap();
    assert_eq!(notification.start_slot, 65_532);
    assert_eq!(notification.parent_block.slot, parent_slot);
    assert_eq!(notification.parent_block.block_id, parent_block_id);
}

#[test]
fn test_process_set_root_command_requires_matching_frozen_bank() {
    let AlpenglowReplayComponents {
        blockstore,
        bank_forks,
        mut progress,
        ..
    } = alpenglow_replay_components(tr(0) / tr(1));
    let root_bank = bank_forks.read().unwrap().root_bank();
    let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&root_bank));
    let (drop_bank_sender, _drop_bank_receiver) = bounded(1024);
    let (votor_commitment_sender, votor_commitment_receiver) = bounded(1024);
    let context = ProcessBankForksContext {
        bank_forks: bank_forks.clone(),
        blockstore: blockstore.clone(),
        snapshot_controller: None,
        bank_notification_sender: None,
        rpc_subscriptions: None,
        drop_bank_sender,
        leader_schedule_cache,
        votor_commitment_sender,
    };
    let my_pubkey = Pubkey::new_unique();

    for command in [
        SetRootCommand {
            new_root: Block::new_unique(2),
        },
        SetRootCommand {
            new_root: Block::new_unique(1),
        },
    ] {
        ReplayStage::process_set_root_command(command, &context, &my_pubkey, &mut progress);
        assert_eq!(bank_forks.read().unwrap().root(), 0);
    }

    let unfrozen_block_id = BlockId::new_unique();
    let unfrozen_bank = Bank::new_from_parent(root_bank, SlotLeader::default(), 2);
    unfrozen_bank.set_block_id(Some(unfrozen_block_id));
    bank_forks.write().unwrap().insert(unfrozen_bank);
    ReplayStage::process_set_root_command(
        SetRootCommand {
            new_root: Block {
                slot: 2,
                block_id: unfrozen_block_id,
            },
        },
        &context,
        &my_pubkey,
        &mut progress,
    );
    assert_eq!(bank_forks.read().unwrap().root(), 0);
    assert!(votor_commitment_receiver.is_empty());

    let block_id = bank_forks.read().unwrap().block_id(1).unwrap();
    ReplayStage::process_set_root_command(
        SetRootCommand {
            new_root: Block { slot: 1, block_id },
        },
        &context,
        &my_pubkey,
        &mut progress,
    );
    assert_eq!(bank_forks.read().unwrap().root(), 1);
    assert!(blockstore.is_root(1));
    assert_eq!(
        votor_commitment_receiver.try_recv().unwrap(),
        CommitmentAggregationData {
            commitment_type: CommitmentType::Rooted,
            slot: 1,
        }
    );
}

#[test]
fn test_dead_fork_transaction_error() {
    let keypair1 = Keypair::new();
    let keypair2 = Keypair::new();
    let missing_keypair = Keypair::new();
    let missing_keypair2 = Keypair::new();

    let result = check_dead_fork(|_funded_keypair, bank| {
        let blockhash = bank.last_blockhash();
        let entry = entry::next_entry(
            &blockhash,
            1,
            vec![
                system_transaction::transfer(&keypair1, &keypair2.pubkey(), 2, blockhash),
                system_transaction::transfer(
                    &missing_keypair,
                    &missing_keypair2.pubkey(),
                    2,
                    blockhash,
                ),
                system_transaction::transfer(&keypair1, &keypair2.pubkey(), 2, Hash::new_unique()),
            ],
        );
        alpenglow_entry_shreds(&bank, &[entry], false)
    });

    assert_matches!(
        result,
        Err(BlockstoreProcessorError::InvalidTransaction(
            TransactionError::BlockhashNotFound
        ))
    );
}

#[test]
fn test_dead_fork_entry_verification_failure() {
    let recipient = Keypair::new();
    let result = check_dead_fork(|funded_keypair, bank| {
        let blockhash = bank.last_blockhash();
        let bad_hash = hash(&[2; 30]);
        let entry = entry::next_entry(
            &bad_hash,
            1,
            vec![system_transaction::transfer(
                funded_keypair,
                &recipient.pubkey(),
                2,
                blockhash,
            )],
        );
        alpenglow_entry_shreds(&bank, &[entry], false)
    });

    assert_matches!(
        result,
        Err(BlockstoreProcessorError::InvalidBlock(
            BlockError::InvalidEntryHash
        ))
    );
}

#[test]
fn test_dead_fork_invalid_tick_hash_count() {
    let recipient = Pubkey::new_unique();
    let result = check_dead_fork(|funded_keypair, bank| {
        let blockhash = bank.last_blockhash();
        let tx = system_transaction::transfer(funded_keypair, &recipient, 1, blockhash);
        let invalid_entry = Entry::new(&blockhash, 2, vec![tx]);
        alpenglow_entry_shreds(&bank, &[invalid_entry], false)
    });

    assert_matches!(
        result,
        Err(BlockstoreProcessorError::InvalidBlock(
            BlockError::InvalidTickHashCount
        ))
    );
}

#[test]
fn test_dead_fork_invalid_slot_tick_count() {
    let result = check_dead_fork(|_funded_keypair, bank| {
        let blockhash = bank.last_blockhash();
        let first_tick = entry::next_entry(&blockhash, 1, vec![]);
        let second_tick = entry::next_entry(&first_tick.hash, 1, vec![]);
        alpenglow_entry_shreds(&bank, &[first_tick, second_tick], false)
    });
    assert_matches!(
        result,
        Err(BlockstoreProcessorError::InvalidBlock(
            BlockError::TooManyTicks
        ))
    );

    let result = check_dead_fork(|funded_keypair, bank| {
        let blockhash = bank.last_blockhash();
        let tx = system_transaction::transfer(funded_keypair, &Pubkey::new_unique(), 1, blockhash);
        let entry = entry::next_entry(&blockhash, 1, vec![tx]);
        alpenglow_entry_shreds(&bank, &[entry], true)
    });
    assert_matches!(
        result,
        Err(BlockstoreProcessorError::InvalidBlock(
            BlockError::TooFewTicks
        ))
    );
}

#[test]
fn test_dead_fork_invalid_last_tick() {
    let result = check_dead_fork(|_funded_keypair, bank| {
        let blockhash = bank.last_blockhash();
        let alpentick = entry::next_entry(&blockhash, 1, vec![]);
        alpenglow_entry_shreds(&bank, &[alpentick], false)
    });

    assert_matches!(
        result,
        Err(BlockstoreProcessorError::InvalidBlock(
            BlockError::InvalidLastTick
        ))
    );
}

#[test]
fn test_dead_fork_trailing_entry() {
    let recipient = Keypair::new();
    let result = check_dead_fork(|funded_keypair, bank| {
        let blockhash = bank.last_blockhash();
        let alpentick = entry::next_entry(&blockhash, 1, vec![]);
        let tx = system_transaction::transfer(funded_keypair, &recipient.pubkey(), 2, blockhash);
        let trailing_entry = entry::next_entry(&alpentick.hash, 1, vec![tx]);
        alpenglow_entry_shreds(&bank, &[alpentick, trailing_entry], true)
    });

    assert_matches!(
        result,
        Err(BlockstoreProcessorError::InvalidBlock(
            BlockError::TrailingEntry
        ))
    );
}

#[test]
fn test_dead_fork_entry_deserialize_failure() {
    let result = check_dead_fork(|_, bank| alpenglow_raw_shreds(&bank, &[0xa5; 1024], true));

    assert_matches!(
        result,
        Err(BlockstoreProcessorError::FailedToLoadEntries(
            BlockstoreError::InvalidShredData(_)
        ))
    );
}

enum CompleteBankFailure {
    ReplayError,
    VerifyError,
}

fn do_test_dead_slot_on_complete_bank(failure: CompleteBankFailure) {
    let AlpenglowReplayComponents {
        blockstore,
        bank_forks,
        mut progress,
        mint_keypair,
        ..
    } = alpenglow_replay_components(tr(0));
    bank_forks
        .write()
        .unwrap()
        .install_scheduler_pool(DefaultSchedulerPool::new_for_verification(
            None, None, None, None, None,
        ));
    let bank0 = bank_forks.read().unwrap().get(0).unwrap();
    let child_bank = Bank::new_from_parent(bank0, SlotLeader::default(), 1);
    blockstore_processor::set_alpenglow_ticks(
        &child_bank,
        &bank_forks.read().unwrap().migration_status(),
    );
    let bank = bank_forks.write().unwrap().insert(child_bank);
    let slot = bank.slot();
    let (finalization_cert_sender, _finalization_cert_receiver) = bounded(1024);
    let context = ProcessActiveBanksContext::new_for_tests(bank_forks.clone(), blockstore.clone());

    let bank_progress = progress
        .entry(slot)
        .or_insert_with(|| ForkProgress::new(bank.last_blockhash(), 0, 0, None));
    let replay_result = match failure {
        CompleteBankFailure::ReplayError => {
            let tx = system_transaction::transfer(
                &mint_keypair,
                &Keypair::new().pubkey(),
                1,
                Hash::new_unique(),
            );
            let entry = entry::next_entry(&bank.last_blockhash(), 1, vec![tx]);
            blockstore
                .insert_shreds(alpenglow_entry_shreds(&bank, &[entry], true), false)
                .unwrap();
            ReplaySlotFromBlockstore {
                is_slot_dead: false,
                bank_slot: slot,
                replay_result: Some(Err(BlockstoreProcessorError::InvalidTransaction(
                    TransactionError::BlockhashNotFound,
                ))),
            }
        }
        CompleteBankFailure::VerifyError => {
            let mut tx = system_transaction::transfer(
                &mint_keypair,
                &Pubkey::new_unique(),
                1,
                bank.last_blockhash(),
            );
            tx.signatures[0] = Signature::default();
            blockstore
                .insert_shreds(alpenglow_complete_slot_shreds(&bank, vec![tx]), false)
                .unwrap();
            let replay_result = ReplayStage::replay_blockstore_into_bank(
                0,
                &context,
                &bank,
                &bank_progress.replay_stats,
                &bank_progress.replay_progress,
                &finalization_cert_sender,
            );
            // Verification runs concurrently with replay, so the invalid signature may be
            // observed here or while completing the bank below.
            assert_matches!(
                replay_result,
                Ok(1)
                    | Err(BlockstoreProcessorError::InvalidTransaction(
                        TransactionError::SignatureFailure
                    ))
            );
            ReplaySlotFromBlockstore {
                is_slot_dead: false,
                bank_slot: slot,
                replay_result: Some(replay_result),
            }
        }
    };

    assert!(blockstore.meta(slot).unwrap().unwrap().is_full());
    assert!(!blockstore.is_dead(slot));
    assert!(progress.get(&slot).unwrap().dead_reason.is_none());

    ReplayStage::process_replay_results(
        &context,
        &mut progress,
        &mut Vec::new(),
        &[replay_result],
        &Pubkey::default(),
    )
    .unwrap();

    assert_eq!(
        progress.get(&slot).unwrap().dead_reason,
        Some(DeadSlotReason::Hard)
    );
    assert!(blockstore.is_dead(slot));
    bank.quiesce_transaction_execution();
    let _ = bank.wait_for_completed_scheduler();
}

#[test]
fn test_dead_slot_on_complete_bank_replay_err() {
    do_test_dead_slot_on_complete_bank(CompleteBankFailure::ReplayError);
}

#[test]
fn test_dead_slot_on_complete_bank_verify_err() {
    do_test_dead_slot_on_complete_bank(CompleteBankFailure::VerifyError);
}

#[test]
fn test_complete_replay_verification_recycles_async_verification() {
    let replay_stats = RwLock::new(ReplaySlotStats::default());
    {
        let mut replay_stats = replay_stats.write().unwrap();
        replay_stats.poh_verify_elapsed = 11;
        replay_stats.transaction_verify_elapsed = 17;
    }
    let replay_progress = RwLock::new(ConfirmationProgress::new_with_async_verification(
        Hash::new_unique(),
        Some(AsyncVerificationProgress::new(1)),
    ));
    let mut async_verification_freelist = Vec::new();

    assert_matches!(
        ReplayStage::complete_replay_verification(
            &replay_stats,
            &replay_progress,
            &mut async_verification_freelist,
        ),
        Ok(())
    );
    assert_eq!(async_verification_freelist.len(), 1);
    assert!(
        replay_progress
            .write()
            .unwrap()
            .take_async_verification()
            .is_none()
    );
    let replay_stats = replay_stats.read().unwrap();
    assert_eq!(replay_stats.poh_verify_elapsed, 11);
    assert_eq!(replay_stats.transaction_verify_elapsed, 17);
}

#[test]
fn test_complete_bank_replay_recycles_async_verification() {
    let AlpenglowReplayComponents { bank_forks, .. } = alpenglow_replay_components(tr(0));
    let bank = bank_forks.read().unwrap().get_with_scheduler(0).unwrap();
    let mut bank_progress = ForkProgress::new(
        bank.last_blockhash(),
        0,
        0,
        Some(AsyncVerificationProgress::new(1)),
    );
    let mut async_verification_freelist = Vec::new();

    let completed_replay = ReplayStage::complete_bank_replay(
        &bank,
        &mut bank_progress,
        &mut async_verification_freelist,
    )
    .unwrap();

    assert!(!completed_replay.is_unified_scheduler_enabled);
    assert!(Arc::ptr_eq(
        &completed_replay.replay_stats,
        &bank_progress.replay_stats
    ));
    assert!(Arc::ptr_eq(
        &completed_replay.replay_progress,
        &bank_progress.replay_progress
    ));
    assert_eq!(async_verification_freelist.len(), 1);
}

#[test]
fn test_write_persist_transaction_status() {
    let GenesisConfigInfo {
        mut genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(solana_native_token::LAMPORTS_PER_SOL * 1000);
    genesis_config.rent.lamports_per_byte = 50;
    let (ledger_path, _) = create_new_tmp_ledger!(&genesis_config);
    {
        let blockstore =
            Blockstore::open(&ledger_path).expect("Expected to successfully open database ledger");
        let blockstore = Arc::new(blockstore);

        let keypair1 = Keypair::new();
        let keypair2 = Keypair::new();
        let keypair3 = Keypair::new();

        let (bank0, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        bank0
            .transfer(
                bank0.get_minimum_balance_for_rent_exemption(0),
                &mint_keypair,
                &keypair2.pubkey(),
            )
            .unwrap();

        let child_bank = Bank::new_from_parent(bank0, SlotLeader::default(), 1);
        let bank1 = bank_forks
            .write()
            .unwrap()
            .insert(child_bank)
            .clone_without_scheduler();
        let slot = bank1.slot();

        let (entries, test_signatures) = create_test_transaction_entries(
            vec![&mint_keypair, &keypair1, &keypair2, &keypair3],
            bank1.clone(),
        );
        populate_blockstore_for_tests(
            entries,
            bank1,
            blockstore.clone(),
            Arc::new(AtomicU64::default()),
        );

        let mut test_signatures_iter = test_signatures.into_iter();
        let confirmed_block = blockstore.get_rooted_block(slot, false).unwrap();
        let actual_tx_results: Vec<_> = confirmed_block
            .transactions
            .into_iter()
            .map(|VersionedTransactionWithStatusMeta { transaction, meta }| {
                (transaction.signatures[0], meta.status)
            })
            .collect();
        let expected_tx_results = vec![
            (test_signatures_iter.next().unwrap(), Ok(())),
            (
                test_signatures_iter.next().unwrap(),
                Err(TransactionError::InstructionError(
                    0,
                    InstructionError::Custom(1),
                )),
            ),
        ];
        assert_eq!(actual_tx_results, expected_tx_results);
        assert!(test_signatures_iter.next().is_none());
    }
    Blockstore::destroy(&ledger_path).unwrap();
}

#[test]
fn test_cmr_mismatch_hard_dead() {
    let AlpenglowReplayComponents {
        blockstore,
        bank_forks,
        mut progress,
        ..
    } = alpenglow_replay_components(tr(0) / tr(1));
    let slot = 1;
    let context = ProcessActiveBanksContext::new_for_tests(bank_forks.clone(), blockstore.clone());
    let replay_result = ReplaySlotFromBlockstore {
        is_slot_dead: false,
        bank_slot: slot,
        replay_result: Some(Err(BlockstoreProcessorError::ChainedBlockIdFailure(
            slot, 0,
        ))),
    };

    ReplayStage::process_replay_results(
        &context,
        &mut progress,
        &mut Vec::new(),
        &[replay_result],
        &Pubkey::new_unique(),
    )
    .unwrap();

    assert!(blockstore.is_dead(slot));
    assert_eq!(
        progress.get(&slot).unwrap().dead_reason,
        Some(DeadSlotReason::Hard)
    );
}

#[test]
fn test_clear_slots_clears_status_cache_for_removed_bank() {
    let AlpenglowReplayComponents {
        bank_forks,
        mint_keypair,
        mut progress,
        ..
    } = alpenglow_replay_components(tr(0));
    let bank0 = bank_forks.read().unwrap().get(0).unwrap();
    let bank1 = Bank::new_from_parent(bank0, SlotLeader::default(), 1);
    let transfer_signature = bank1
        .transfer(1, &mint_keypair, &Pubkey::new_unique())
        .unwrap();
    bank_forks.write().unwrap().insert(bank1);
    let bank1 = bank_forks.read().unwrap().get(1).unwrap();
    assert!(bank1.get_signature_status(&transfer_signature).is_some());

    let removed_bank = bank_forks.write().unwrap().remove(1).unwrap();
    drop(removed_bank);
    assert!(bank_forks.read().unwrap().get(1).is_none());
    assert!(bank1.get_signature_status(&transfer_signature).is_some());

    ReplayStage::clear_slots([1], &bank_forks, &mut progress, &mut Vec::new(), None).unwrap();

    assert!(bank1.get_signature_status(&transfer_signature).is_none());
}

#[test]
fn test_clear_slots_propagates_transaction_status_disconnect() {
    let AlpenglowReplayComponents {
        bank_forks,
        mut progress,
        ..
    } = alpenglow_replay_components(tr(0));
    let bank0 = bank_forks.read().unwrap().get(0).unwrap();
    let bank1 = Bank::new_from_parent(bank0, SlotLeader::default(), 1);
    bank_forks.write().unwrap().insert(bank1);

    let (sender, receiver) = bounded(1);
    drop(receiver);
    let transaction_status_sender = TransactionStatusSender {
        sender,
        dependency_tracker: None,
    };

    for (source, purge_input) in [
        (
            TransactionHistoryPurgeSource::UpdateParentSignal,
            TransactionHistoryPurgeInput::ReplayStage,
        ),
        (
            TransactionHistoryPurgeSource::SwitchBank,
            TransactionHistoryPurgeInput::SwitchBank,
        ),
    ] {
        assert_matches!(
            ReplayStage::clear_slots(
                [1],
                &bank_forks,
                &mut progress,
                &mut Vec::new(),
                Some((&transaction_status_sender, source, purge_input)),
            ),
            Err(ReplayStageError::PurgeTransactionHistory { slot: 1, .. })
        );
    }
}

#[test]
fn test_switch_bank_purges_cleared_descendants() {
    let AlpenglowReplayComponents {
        blockstore,
        bank_forks,
        mut progress,
        ..
    } = alpenglow_replay_components(tr(0) / tr(1));
    let bank1 = bank_forks.read().unwrap().get(1).unwrap();
    let leader_bank = Bank::new_from_parent(bank1, SlotLeader::default(), 2).mark_leader_bank();
    leader_bank.freeze();
    let leader_bank = bank_forks.write().unwrap().insert(leader_bank);
    let block_id = Hash::new_unique();
    blockstore
        .set_double_merkle_root(1, BlockLocation::Original, block_id)
        .unwrap();
    blockstore.set_dead_slot(1).unwrap();

    let latest_switch_request = LatestSwitchRequest::default();
    assert!(
        latest_switch_request
            .try_advance(SwitchBankEvent::Switch {
                block: Block {
                    slot: 1,
                    block_id: block_id.into(),
                },
            })
            .is_none()
    );
    let (sender, receiver) = bounded(2);
    let purge_responder = std::thread::spawn(move || {
        receiver
            .into_iter()
            .map(|message| {
                let TransactionStatusMessage::PurgeTransactionHistory {
                    slot,
                    source,
                    purge_input,
                    done_sender,
                    ..
                } = message
                else {
                    panic!("expected transaction-history purge request");
                };
                assert_eq!(source, TransactionHistoryPurgeSource::SwitchBank);
                assert!(matches!(
                    purge_input,
                    TransactionHistoryPurgeInput::SwitchBank
                ));
                done_sender.unwrap().send(()).unwrap();
                slot
            })
            .collect::<Vec<_>>()
    });
    let transaction_status_sender = TransactionStatusSender {
        sender,
        dependency_tracker: None,
    };
    let mut pending_switch = None;

    ReplayStage::process_switch_bank_events(
        &Pubkey::new_unique(),
        &latest_switch_request,
        &mut pending_switch,
        &blockstore,
        &bank_forks,
        &mut progress,
        &mut Vec::new(),
        Some(&transaction_status_sender),
    )
    .unwrap();

    assert!(pending_switch.is_some());
    assert!(bank_forks.read().unwrap().get(1).is_some());
    assert!(bank_forks.read().unwrap().get(2).is_some());

    blockstore.insert_shreds_for_bank(leader_bank.clone_without_scheduler());
    ReplayStage::process_switch_bank_events(
        &Pubkey::new_unique(),
        &latest_switch_request,
        &mut pending_switch,
        &blockstore,
        &bank_forks,
        &mut progress,
        &mut Vec::new(),
        Some(&transaction_status_sender),
    )
    .unwrap();

    drop(transaction_status_sender);
    assert_eq!(purge_responder.join().unwrap(), vec![1, 2]);
    assert!(pending_switch.is_none());
    assert!(bank_forks.read().unwrap().get(1).is_none());
    assert!(bank_forks.read().unwrap().get(2).is_none());
    assert!(!blockstore.is_dead(1));
}

#[test]
fn test_update_parent_restart() {
    let AlpenglowReplayComponents {
        blockstore,
        bank_forks,
        mut progress,
        ..
    } = alpenglow_replay_components(tr(0) / tr(1) / tr(2) / tr(3));
    let bank0 = bank_forks.read().unwrap().get(0).unwrap();

    // Only slot 4 is before its UpdateParent replay boundary and eligible to restart.
    for (slot, shreds) in [(4, 5), (8, 40), (12, 32), (16, 32)] {
        let bank = Bank::new_from_parent(bank0.clone(), SlotLeader::default(), slot);
        bank_forks.write().unwrap().insert(bank);
        let mut fork_progress = ForkProgress::new(Hash::default(), 0, 0, None);
        fork_progress.replay_progress.write().unwrap().num_shreds = shreds;
        if slot == 16 {
            fork_progress.mark_dead(DeadSlotReason::ReplayFailureAtUpdateParent(64));
        }
        progress.insert(slot, fork_progress);
    }

    let (sender, receiver) = bounded(1024);
    for slot in [4, 8, 12, 16] {
        insert_update_parent_slot(&blockstore, slot, 3, 0, BlockId::new_unique(), 32);
        sender.send(UpdateParentSignal { slot }).unwrap();
    }

    let cleared_bank_id = bank_forks.read().unwrap().get(4).unwrap().bank_id();
    let (entry_notification_sender, entry_notification_receiver) = bounded(1);
    let (transaction_status_sender, purge_response_thread) = transaction_history_purge_responder();
    handle_update_parent_interrupts(
        &Pubkey::new_unique(),
        &blockstore,
        &bank_forks,
        &mut progress,
        &mut Vec::new(),
        &receiver,
        Some(&entry_notification_sender),
        Some(&transaction_status_sender),
    )
    .unwrap();

    assert_eq!(
        purge_response_thread.join().unwrap(),
        (4, TransactionHistoryPurgeSource::UpdateParentSignal)
    );
    assert!(progress.get(&4).is_none());
    assert!(progress.get(&8).is_some());
    assert!(progress.get(&12).is_some());
    assert!(progress.get(&16).is_some());

    let EntryNotification::UpdateParent(update_parent) =
        entry_notification_receiver.try_recv().unwrap()
    else {
        panic!("expected UpdateParent entry notification");
    };
    let slot_meta = blockstore.meta(4).unwrap().unwrap();
    assert_eq!(update_parent.slot, 4);
    assert_eq!(update_parent.cleared_bank_id, cleared_bank_id);
    assert_eq!(update_parent.parent_slot, slot_meta.parent_slot.unwrap());
    assert_eq!(
        update_parent.parent_block_id.to_hash(),
        slot_meta.parent_block_id
    );
}

#[test_case(false; "transaction_status_connected")]
#[test_case(true; "transaction_status_disconnected")]
fn test_headerless_update_parent(transaction_status_disconnected: bool) {
    let AlpenglowReplayComponents {
        blockstore,
        bank_forks,
        mut progress,
        my_pubkey,
        leader_schedule_cache,
        ..
    } = alpenglow_replay_components(tr(0));
    let slot = 4;

    let footer_marker = || {
        VersionedBlockMarker::from_block_footer(BlockFooterV1 {
            bank_hash: Hash::new_unique(),
            block_producer_time_nanos: 0,
            block_user_agent: vec![],
            block_final_cert: None,
            skip_reward_cert: None,
            notar_reward_cert: None,
        })
    };
    let update_parent = VersionedBlockMarker::from_update_parent(UpdateParentV1 {
        new_parent_slot: 0,
        new_parent_block_id: BlockId::default(),
    });

    let mut shreds = block_marker_shreds(slot, 0, footer_marker(), 0);
    shreds.extend(block_marker_shreds(slot, 0, update_parent, 32));
    shreds.extend(block_marker_shreds_with_last(
        slot,
        0,
        footer_marker(),
        64,
        true,
    ));
    blockstore.insert_shreds(shreds, true).unwrap();
    let slot_meta = blockstore.meta(slot).unwrap().unwrap();
    assert!(blockstore.is_full(slot), "{slot_meta:?}");
    assert!(slot_meta.has_update_parent());
    assert!(bank_forks.read().unwrap().get(slot).is_none());

    let (sender, receiver) = bounded(1);
    let receiver = (!transaction_status_disconnected).then_some(receiver);
    let transaction_status_sender = TransactionStatusSender {
        sender,
        dependency_tracker: None,
    };
    let result = ReplayStage::generate_new_bank_forks(
        NewBankForksContext {
            blockstore: &blockstore,
            bank_forks: &bank_forks,
            leader_schedule_cache: &leader_schedule_cache,
            rpc_subscriptions: None,
            slot_status_notifier: &None,
            transaction_status_sender: Some(&transaction_status_sender),
            my_pubkey: &my_pubkey,
        },
        &mut progress,
        &mut ReplayLoopTiming::default(),
    );
    if transaction_status_disconnected {
        assert_matches!(
            result,
            Err(ReplayStageError::PurgeTransactionHistory { slot: 4, .. })
        );
        assert!(bank_forks.read().unwrap().get(slot).is_none());
        assert!(!progress.contains_key(&slot));
        return;
    }
    result.unwrap();
    assert_matches!(
        receiver.unwrap().try_recv(),
        Ok(TransactionStatusMessage::PurgeTransactionHistory {
            slot: 4,
            source: TransactionHistoryPurgeSource::ReplayStart,
            purge_input: TransactionHistoryPurgeInput::ReplayStage,
            done_sender: None,
            ..
        })
    );
    assert!(bank_forks.read().unwrap().get(slot).is_some());
    let replay_progress = progress.get(&slot).unwrap().replay_progress.read().unwrap();
    assert_eq!(replay_progress.num_shreds, 32);
    assert_eq!(replay_progress.num_txs, 0);
}

#[test]
fn test_abandon_invalidates() {
    let AlpenglowReplayComponents {
        blockstore,
        bank_forks,
        mut progress,
        ..
    } = alpenglow_replay_components(tr(0) / tr(1));
    let slot = 4;
    let parent_block_id = BlockId::new_unique();
    insert_update_parent_slot(&blockstore, slot, 3, 0, parent_block_id, 32);
    let bank0 = bank_forks.read().unwrap().get(0).unwrap();
    let bank = Bank::new_from_parent(bank0, SlotLeader::default(), slot);
    bank_forks.write().unwrap().insert(bank);
    let bank = bank_forks.read().unwrap().get(slot).unwrap();
    progress.insert(slot, ForkProgress::new(bank.last_blockhash(), 0, 0, None));

    let mut context = ProcessActiveBanksContext::new_for_tests(bank_forks.clone(), blockstore);
    let (entry_notification_sender, entry_notification_receiver) = bounded(1);
    context.entry_notification_sender = Some(entry_notification_sender);
    let (transaction_status_sender, purge_response_thread) = transaction_history_purge_responder();
    context.transaction_status_sender = Some(transaction_status_sender);
    let update_parent = VersionedUpdateParent::V1(UpdateParentV1 {
        new_parent_slot: 0,
        new_parent_block_id: parent_block_id,
    });
    let replay_result = ReplaySlotFromBlockstore {
        is_slot_dead: false,
        bank_slot: slot,
        replay_result: Some(Err(BlockstoreProcessorError::BlockComponentProcessor(
            BlockComponentProcessorError::AbandonedBank(update_parent),
        ))),
    };

    ReplayStage::process_replay_results(
        &context,
        &mut progress,
        &mut Vec::new(),
        &[replay_result],
        &Pubkey::new_unique(),
    )
    .unwrap();

    assert_eq!(
        purge_response_thread.join().unwrap(),
        (slot, TransactionHistoryPurgeSource::AbandonedBank)
    );
    assert!(progress.get(&slot).is_none());
    assert!(bank_forks.read().unwrap().get(slot).is_none());
    let EntryNotification::UpdateParent(update_parent) =
        entry_notification_receiver.try_recv().unwrap()
    else {
        panic!("expected UpdateParent entry notification");
    };
    assert_eq!(update_parent.slot, slot);
    assert_eq!(update_parent.cleared_bank_id, bank.bank_id());
    assert_eq!(update_parent.parent_slot, 0);
    assert_eq!(update_parent.parent_block_id, parent_block_id);
}

#[test]
fn test_update_parent_interrupt_ignores_non_first_leader_window_slot() {
    let AlpenglowReplayComponents {
        blockstore,
        bank_forks,
        mut progress,
        ..
    } = alpenglow_replay_components(tr(0) / tr(1));
    let slot = 1;
    let mut meta = blockstore.meta(slot).unwrap().unwrap();
    meta.parent_slot = Some(0);
    meta.parent_block_id = Hash::new_unique();
    meta.replay_fec_set_index = 10;
    blockstore.put_meta(slot, &meta).unwrap();

    let fork_progress = ForkProgress::new(Hash::default(), 0, 0, None);
    fork_progress.replay_progress.write().unwrap().num_shreds = 5;
    progress.insert(slot, fork_progress);
    let (sender, receiver) = bounded(1);
    sender.send(UpdateParentSignal { slot }).unwrap();

    handle_update_parent_interrupts(
        &Pubkey::new_unique(),
        &blockstore,
        &bank_forks,
        &mut progress,
        &mut Vec::new(),
        &receiver,
        None,
        None,
    )
    .unwrap();

    assert!(progress.get(&slot).is_some());
    assert!(bank_forks.read().unwrap().get(slot).is_some());
}

#[test]
fn test_update_parent_keeps_hard() {
    let AlpenglowReplayComponents {
        blockstore,
        bank_forks,
        mut progress,
        ..
    } = alpenglow_replay_components(tr(0) / tr(1));
    let slot = 1;
    let mut meta = blockstore.meta(slot).unwrap().unwrap();
    meta.parent_slot = Some(0);
    meta.parent_block_id = Hash::new_unique();
    meta.replay_fec_set_index = 10;
    blockstore.put_meta(slot, &meta).unwrap();
    blockstore.set_dead_slot(slot).unwrap();

    let mut fork_progress = ForkProgress::new(Hash::default(), 0, 0, None);
    fork_progress.replay_progress.write().unwrap().num_shreds = 5;
    fork_progress.mark_dead(DeadSlotReason::Hard);
    progress.insert(slot, fork_progress);
    let (sender, receiver) = bounded(1);
    sender.send(UpdateParentSignal { slot }).unwrap();

    handle_update_parent_interrupts(
        &Pubkey::new_unique(),
        &blockstore,
        &bank_forks,
        &mut progress,
        &mut Vec::new(),
        &receiver,
        None,
        None,
    )
    .unwrap();

    assert!(blockstore.is_dead(slot));
    assert!(progress.get(&slot).is_some());
    assert!(bank_forks.read().unwrap().get(slot).is_some());
}

#[test]
fn test_pre_update_soft_dead() {
    let AlpenglowReplayComponents {
        blockstore,
        bank_forks,
        mut progress,
        ..
    } = alpenglow_replay_components(tr(0) / tr(1));
    let slot = 1;
    let mut meta = blockstore.meta(slot).unwrap().unwrap();
    meta.replay_fec_set_index = 0;
    meta.last_index = None;
    blockstore.put_meta(slot, &meta).unwrap();

    let bank = bank_forks.read().unwrap().get(slot).unwrap();
    let fork_progress = ForkProgress::new(bank.last_blockhash(), 0, 0, None);
    fork_progress.replay_progress.write().unwrap().num_shreds = 5;
    progress.insert(slot, fork_progress);
    mark_replay_dead_slot(
        &bank,
        &BlockstoreProcessorError::InvalidTransaction(TransactionError::AccountNotFound),
        &mut progress,
        &dead_slot_context_for_tests(blockstore.clone()),
    );

    assert!(!blockstore.is_dead(slot));
    assert_eq!(
        progress.get(&slot).unwrap().dead_reason,
        Some(DeadSlotReason::ReplayFailureBeforeUpdateParent)
    );
}

#[test]
fn test_after_update_hard_dead() {
    let AlpenglowReplayComponents {
        blockstore,
        bank_forks,
        mut progress,
        ..
    } = alpenglow_replay_components(tr(0) / tr(1));
    let slot = 1;
    let mut meta = blockstore.meta(slot).unwrap().unwrap();
    meta.replay_fec_set_index = 5;
    meta.last_index = None;
    blockstore.put_meta(slot, &meta).unwrap();

    let bank = bank_forks.read().unwrap().get(slot).unwrap();
    let fork_progress = ForkProgress::new(bank.last_blockhash(), 0, 0, None);
    fork_progress.replay_progress.write().unwrap().num_shreds = 5;
    progress.insert(slot, fork_progress);
    mark_replay_dead_slot(
        &bank,
        &BlockstoreProcessorError::InvalidTransaction(TransactionError::AccountNotFound),
        &mut progress,
        &dead_slot_context_for_tests(blockstore.clone()),
    );

    assert!(blockstore.is_dead(slot));
    assert_eq!(
        progress.get(&slot).unwrap().dead_reason,
        Some(DeadSlotReason::Hard)
    );
}

#[test_case(31, false; "before_update_parent_restarts")]
#[test_case(32, false; "at_update_parent_restarts")]
#[test_case(33, true; "after_update_parent_is_hard_dead")]
fn test_spurious_update_parent_boundary(replayed_shreds: u64, should_be_hard: bool) {
    let AlpenglowReplayComponents {
        blockstore,
        bank_forks,
        mut progress,
        ..
    } = alpenglow_replay_components(tr(0));
    let slot = 4;
    let bank0 = bank_forks.read().unwrap().get(0).unwrap();
    let bank = Bank::new_from_parent(bank0, SlotLeader::default(), slot);
    let bank = bank_forks.write().unwrap().insert(bank);
    let header = VersionedBlockMarker::from_block_header(BlockHeaderV1 {
        parent_slot: 0,
        parent_block_id: BlockId::default(),
    });
    let update_parent = VersionedBlockMarker::from_update_parent(UpdateParentV1 {
        new_parent_slot: 0,
        new_parent_block_id: BlockId::default(),
    });
    let mut shreds = block_marker_shreds(slot, 0, header, 0);
    shreds.retain(|shred| !shred.is_data() || shred.index() != 0);
    shreds.extend(block_marker_shreds(slot, 0, update_parent, 32));
    blockstore.insert_shreds(shreds, true).unwrap();
    let meta = blockstore.meta(slot).unwrap().unwrap();
    assert!(meta.has_update_parent());
    let replay_fec_set_index = u64::from(meta.replay_fec_set_index);
    assert_eq!(replay_fec_set_index, 32);

    let fork_progress = ForkProgress::new(bank.last_blockhash(), 0, 0, None);
    fork_progress.replay_progress.write().unwrap().num_shreds = replayed_shreds;
    progress.insert(slot, fork_progress);
    mark_replay_dead_slot(
        &bank,
        &BlockstoreProcessorError::BlockComponentProcessor(
            BlockComponentProcessorError::SpuriousUpdateParent,
        ),
        &mut progress,
        &dead_slot_context_for_tests(blockstore.clone()),
    );

    if should_be_hard {
        assert!(blockstore.is_dead(slot));
        assert_eq!(
            progress.get(&slot).unwrap().dead_reason,
            Some(DeadSlotReason::Hard)
        );
        return;
    }

    assert!(!blockstore.is_dead(slot));
    let expected_reason = if replayed_shreds < replay_fec_set_index {
        DeadSlotReason::ReplayFailureBeforeUpdateParent
    } else {
        DeadSlotReason::ReplayFailureAtUpdateParent(replay_fec_set_index)
    };
    assert_eq!(
        progress.get(&slot).unwrap().dead_reason.as_ref(),
        Some(&expected_reason)
    );
    process_soft_dead_slots(
        &Pubkey::new_unique(),
        &blockstore,
        &bank_forks,
        &None,
        &None,
        &mut progress,
        &mut Vec::new(),
        None,
        None,
    )
    .unwrap();
    assert!(!blockstore.is_dead(slot));
    assert!(progress.get(&slot).is_none());
    assert!(bank_forks.read().unwrap().get(slot).is_none());
}

#[test]
fn test_before_update_soft_dead() {
    let AlpenglowReplayComponents {
        blockstore,
        bank_forks,
        mut progress,
        ..
    } = alpenglow_replay_components(tr(0));
    let slot = 4;
    let bank0 = bank_forks.read().unwrap().get(0).unwrap();
    let bank = Bank::new_from_parent(bank0, SlotLeader::default(), slot);
    let bank = bank_forks.write().unwrap().insert(bank);
    let header = VersionedBlockMarker::from_block_header(BlockHeaderV1 {
        parent_slot: 0,
        parent_block_id: BlockId::default(),
    });
    let update_parent = VersionedBlockMarker::from_update_parent(UpdateParentV1 {
        new_parent_slot: 0,
        new_parent_block_id: BlockId::default(),
    });
    let mut shreds = block_marker_shreds(slot, 0, header, 0);
    shreds.retain(|shred| !shred.is_data() || shred.index() != 0);
    shreds.extend(block_marker_shreds(slot, 0, update_parent, 32));
    blockstore.insert_shreds(shreds, true).unwrap();
    assert!(blockstore.meta(slot).unwrap().unwrap().has_update_parent());

    let fork_progress = ForkProgress::new(bank.last_blockhash(), 0, 0, None);
    fork_progress.replay_progress.write().unwrap().num_shreds = 5;
    progress.insert(slot, fork_progress);
    mark_replay_dead_slot(
        &bank,
        &BlockstoreProcessorError::BlockComponentProcessor(
            BlockComponentProcessorError::MissingParentMarker,
        ),
        &mut progress,
        &dead_slot_context_for_tests(blockstore.clone()),
    );

    assert!(!blockstore.is_dead(slot));
    assert_eq!(
        progress.get(&slot).unwrap().dead_reason,
        Some(DeadSlotReason::ReplayFailureBeforeUpdateParent)
    );
}

#[test]
fn test_soft_dead_restarts() {
    let AlpenglowReplayComponents {
        blockstore,
        bank_forks,
        mut progress,
        ..
    } = alpenglow_replay_components(tr(0) / tr(1));
    let slot = 4;
    insert_update_parent_slot(&blockstore, slot, 3, 0, BlockId::new_unique(), 32);
    let bank0 = bank_forks.read().unwrap().get(0).unwrap();
    let bank = Bank::new_from_parent(bank0, SlotLeader::default(), slot);
    bank_forks.write().unwrap().insert(bank);

    let mut fork_progress = ForkProgress::new(Hash::default(), 0, 0, None);
    fork_progress.replay_progress.write().unwrap().num_shreds = 5;
    fork_progress.mark_dead(DeadSlotReason::ReplayFailureBeforeUpdateParent);
    progress.insert(slot, fork_progress);
    let (transaction_status_sender, purge_response_thread) = transaction_history_purge_responder();
    process_soft_dead_slots(
        &Pubkey::new_unique(),
        &blockstore,
        &bank_forks,
        &None,
        &None,
        &mut progress,
        &mut Vec::new(),
        None,
        Some(&transaction_status_sender),
    )
    .unwrap();

    assert_eq!(
        purge_response_thread.join().unwrap(),
        (slot, TransactionHistoryPurgeSource::SoftDeadSlot)
    );
    assert!(!blockstore.is_dead(slot));
    assert!(progress.get(&slot).is_none());
    assert!(bank_forks.read().unwrap().get(slot).is_none());
}

#[test]
fn test_full_soft_dead_hardens() {
    let AlpenglowReplayComponents {
        blockstore,
        bank_forks,
        mut progress,
        ..
    } = alpenglow_replay_components(tr(0) / tr(1));
    let slot = 1;
    let mut meta = blockstore.meta(slot).unwrap().unwrap();
    meta.replay_fec_set_index = 0;
    meta.consumed = 1;
    meta.last_index = Some(0);
    blockstore.put_meta(slot, &meta).unwrap();

    let mut fork_progress = ForkProgress::new(Hash::default(), 0, 0, None);
    fork_progress.mark_dead(DeadSlotReason::ReplayFailureBeforeUpdateParent);
    progress.insert(slot, fork_progress);
    process_soft_dead_slots(
        &Pubkey::new_unique(),
        &blockstore,
        &bank_forks,
        &None,
        &None,
        &mut progress,
        &mut Vec::new(),
        None,
        None,
    )
    .unwrap();

    assert!(blockstore.is_dead(slot));
    assert_eq!(
        progress.get(&slot).unwrap().dead_reason,
        Some(DeadSlotReason::Hard)
    );
}

#[test]
fn test_replay_own_update_full() {
    let AlpenglowReplayComponents {
        blockstore,
        bank_forks,
        mut progress,
        my_pubkey,
        leader_schedule_cache,
        ..
    } = alpenglow_replay_components(tr(0));
    let slot = 4;
    let replay_fec_set_index = 32;
    insert_update_parent_slot(
        &blockstore,
        slot,
        1,
        0,
        BlockId::default(),
        replay_fec_set_index,
    );
    let mut meta = blockstore.meta(slot).unwrap().unwrap();
    meta.consumed = u64::from(replay_fec_set_index) + 1;
    meta.last_index = Some(u64::from(replay_fec_set_index));
    blockstore.put_meta(slot, &meta).unwrap();
    assert!(blockstore.is_full(slot));

    let root_bank = bank_forks.read().unwrap().root_bank();
    let leader = leader_schedule_cache
        .slot_leader_at(slot, Some(&root_bank))
        .unwrap();
    assert_eq!(leader.id, my_pubkey);

    ReplayStage::generate_new_bank_forks(
        NewBankForksContext {
            blockstore: &blockstore,
            bank_forks: &bank_forks,
            leader_schedule_cache: &leader_schedule_cache,
            rpc_subscriptions: None,
            slot_status_notifier: &None,
            transaction_status_sender: None,
            my_pubkey: &my_pubkey,
        },
        &mut progress,
        &mut ReplayLoopTiming::default(),
    )
    .unwrap();

    let bank = bank_forks.read().unwrap().get(slot).unwrap();
    assert!(bank.should_replay_from_blockstore());
    assert_eq!(
        progress
            .get(&slot)
            .unwrap()
            .replay_progress
            .read()
            .unwrap()
            .num_shreds,
        u64::from(replay_fec_set_index),
    );
}

#[test]
fn test_latest_parent_coalesces() {
    let (sender, receiver) = bounded(1);
    sender
        .try_send(LeaderWindowInfo {
            start_slot: 8,
            end_slot: 11,
            parent_block: Block::new_unique(7),
            block_timer: Instant::now(),
        })
        .unwrap();
    ReplayStage::try_send_latest_optimistic_parent(
        &sender,
        &receiver,
        LeaderWindowInfo {
            start_slot: 12,
            end_slot: 15,
            parent_block: Block::new_unique(11),
            block_timer: Instant::now(),
        },
    );
    assert_eq!(receiver.try_recv().unwrap().start_slot, 12);
    assert!(receiver.try_recv().is_err());

    let (sender, receiver) = bounded(1);
    sender
        .try_send(LeaderWindowInfo {
            start_slot: 20,
            end_slot: 22,
            parent_block: Block::new_unique(19),
            block_timer: Instant::now(),
        })
        .unwrap();
    ReplayStage::try_send_latest_optimistic_parent(
        &sender,
        &receiver,
        LeaderWindowInfo {
            start_slot: 20,
            end_slot: 23,
            parent_block: Block::new_unique(19),
            block_timer: Instant::now(),
        },
    );
    assert_eq!(receiver.try_recv().unwrap().end_slot, 23);
    assert!(receiver.try_recv().is_err());

    let (sender, receiver) = bounded(1);
    sender
        .try_send(LeaderWindowInfo {
            start_slot: 20,
            end_slot: 23,
            parent_block: Block::new_unique(19),
            block_timer: Instant::now(),
        })
        .unwrap();
    ReplayStage::try_send_latest_optimistic_parent(
        &sender,
        &receiver,
        LeaderWindowInfo {
            start_slot: 16,
            end_slot: 19,
            parent_block: Block::new_unique(15),
            block_timer: Instant::now(),
        },
    );
    assert_eq!(receiver.try_recv().unwrap().start_slot, 20);
    assert!(receiver.try_recv().is_err());
}
