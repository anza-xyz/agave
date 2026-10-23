use {
    super::*,
    crate::{
        commitment_service::AggregateCommitmentService,
        consensus::{
            Tower,
            progress_map::{DeadSlotReason, RETRANSMIT_BASE_DELAY_MS, ValidatorStakeInfo},
        },
        replay_stage::ReplayStage,
        vote_simulator::{self, VoteSimulator},
    },
    agave_votor_messages::{
        certificate::{CertSignature, GenesisCert},
        consensus_message::Block,
    },
    blockstore_processor::{AsyncVerificationProgress, ConfirmationProgress},
    crossbeam_channel::bounded,
    itertools::Itertools,
    solana_account::{ReadableAccount, state_traits::StateMutWincode as _},
    solana_accounts_db::accounts_db::{ACCOUNTS_DB_CONFIG_FOR_TESTING, AccountsDbConfig},
    solana_bls_signatures::{BLS_SIGNATURE_AFFINE_SIZE, Signature as BLSSignature},
    solana_entry::{
        block_component::{
            BlockComponent, BlockFooterV1, BlockHeaderV1, UpdateParentV1, VersionedBlockMarker,
            VersionedUpdateParent,
        },
        entry::{self, Entry},
    },
    solana_epoch_schedule::EpochSchedule,
    solana_genesis_config as genesis_config,
    solana_gossip::node::Node,
    solana_hash::Hash,
    solana_instruction_error::InstructionError,
    solana_keypair::Keypair,
    solana_leader_schedule::NUM_CONSECUTIVE_LEADER_SLOTS as NUM_CONSECUTIVE_LEADER_SLOTS_NZ,
    solana_ledger::{
        blockstore::{UpdateParentSignal, entries_to_test_shreds, make_slot_entries},
        blockstore_meta::BlockLocation,
        create_new_tmp_ledger,
        entry_notifier_service::EntryNotification,
        genesis_utils::{create_genesis_config, create_genesis_config_with_leader},
        get_tmp_ledger_path,
        shred::{ProcessShredsStats, Shred, Shredder},
    },
    solana_net_utils::SocketAddrSpace,
    solana_poh::poh_recorder::create_test_recorder,
    solana_poh_config::PohConfig,
    solana_rpc::{
        optimistically_confirmed_bank_tracker::OptimisticallyConfirmedBank,
        rpc::{create_test_transaction_entries, populate_blockstore_for_tests},
    },
    solana_runtime::{
        bank::BankTestConfig,
        block_component_processor::BlockComponentProcessorError,
        commitment::BlockCommitment,
        genesis_utils::{GenesisConfigInfo, ValidatorVoteKeypairs},
        transaction_execution::{
            TransactionHistoryPurgeInput, TransactionHistoryPurgeSource, TransactionStatusMessage,
        },
    },
    solana_signature::Signature,
    solana_system_transaction as system_transaction,
    solana_transaction_error::TransactionError,
    solana_transaction_status::VersionedTransactionWithStatusMeta,
    solana_unified_scheduler_pool::DefaultSchedulerPool,
    solana_vote_program::vote_state::{
        self, VoteStateV4, VoteStateVersions, handler::VoteStateHandler,
    },
    std::{
        iter,
        sync::{Arc, Barrier, RwLock, atomic::AtomicU64},
    },
    test_case::test_case,
    trees::{Tree, tr},
};

const NUM_CONSECUTIVE_LEADER_SLOTS: Slot = NUM_CONSECUTIVE_LEADER_SLOTS_NZ.get() as Slot;

static_assertions::const_assert!(REFRESH_VOTE_BLOCKHEIGHT < solana_clock::MAX_PROCESSING_AGE);

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

impl ProcessActiveBanksContext {
    fn new_for_tests(
        bank_forks: Arc<RwLock<BankForks>>,
        blockstore: Arc<Blockstore>,
        replay_vote_sender: ReplayVoteSender,
    ) -> Self {
        let (cluster_slots_update_sender, _) = bounded(1024);
        let (cost_update_sender, _) = bounded(1024);
        let (ancestor_hashes_replay_update_sender, _) = bounded(1024);
        let (votor_event_sender, _) = bounded(1024);
        let migration_status = Arc::new(MigrationStatus::default());
        let replay_verification_worker_pool = ReplayVerificationWorkerPool::new(1);
        Self {
            bank_forks,
            blockstore,
            transaction_status_sender: None,
            entry_notification_sender: None,
            replay_vote_sender,
            bank_notification_sender: None,
            rpc_subscriptions: None,
            slot_status_notifier: None,
            cluster_slots_update_sender,
            cost_update_sender,
            ancestor_hashes_replay_update_sender,
            block_metadata_notifier: None,
            votor_event_sender,
            replay_mode: ForkReplayMode::Serial,
            replay_verification_worker_pool,
            migration_status,
        }
    }
}

fn post_migration_status_for_tests() -> MigrationStatus {
    let migration_status = MigrationStatus::default();
    migration_status.record_feature_activation(0);
    let genesis_block = Block::new_unique(0);
    let genesis_certificate = Arc::new(GenesisCert {
        block: genesis_block,
        signature: CertSignature {
            signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
            bitmap: vec![],
        },
    });
    migration_status.set_genesis_block(genesis_block);
    migration_status.set_genesis_certificate(genesis_certificate);
    migration_status.enable_alpenglow_during_startup();
    migration_status
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

fn cluster_info_for_tests() -> ClusterInfo {
    let keypair = Arc::new(Keypair::new());
    let my_pubkey = keypair.pubkey();
    ClusterInfo::new(
        Node::new_localhost_with_pubkey(&my_pubkey).info,
        keypair,
        SocketAddrSpace::Unspecified,
    )
}

fn block_marker_shreds(
    slot: Slot,
    shred_parent_slot: Slot,
    marker: VersionedBlockMarker,
    shred_index: u32,
) -> Vec<Shred> {
    block_marker_shreds_with_last(
        slot,
        shred_parent_slot,
        marker,
        shred_index,
        false, // is_last_in_slot
    )
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
        parent_block_id: Hash::new_unique(),
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

/// Build the minimal dead-slot context needed by focused replay tests.
fn dead_slot_context_for_tests<'a>(
    blockstore: Arc<Blockstore>,
    replay_vote_sender: ReplayVoteSender,
    ancestor_hashes_replay_update_sender: &'a AncestorHashesReplayUpdateSender,
    duplicate_slots_to_repair: &'a mut DuplicateSlotsToRepair,
    purge_repair_slot_counter: &'a mut PurgeRepairSlotCounter,
    tbft_structs: Option<&'a mut TowerBFTStructures>,
    migration_status: &'a MigrationStatus,
) -> DeadSlotContext<'a> {
    DeadSlotContext {
        notifications: DeadSlotNotifications {
            blockstore,
            rpc_subscriptions: None,
            slot_status_notifier: None,
            replay_vote_sender,
        },
        duplicate: DeadSlotDuplicateContext {
            root: 0,
            duplicate_slots_to_repair,
            ancestor_hashes_replay_update_sender,
            purge_repair_slot_counter,
            tbft_structs,
        },
        migration_status,
    }
}

#[test]
fn test_is_partition_detected() {
    let (VoteSimulator { bank_forks, .. }, _) = setup_default_forks(1, None::<GenerateVotes>);
    let ancestors = bank_forks.read().unwrap().ancestors();
    // Last vote 1 is an ancestor of the heaviest slot 3, no partition
    assert!(!ReplayStage::is_partition_detected(&ancestors, 1, 3));
    // Last vote 1 is an ancestor of the from heaviest slot 1, no partition
    assert!(!ReplayStage::is_partition_detected(&ancestors, 3, 3));
    // Last vote 2 is not an ancestor of the heaviest slot 3,
    // partition detected!
    assert!(ReplayStage::is_partition_detected(&ancestors, 2, 3));
    // Last vote 4 is not an ancestor of the heaviest slot 3,
    // partition detected!
    assert!(ReplayStage::is_partition_detected(&ancestors, 4, 3));
}

pub struct ReplayBlockstoreComponents {
    pub blockstore: Arc<Blockstore>,
    validator_node_to_vote_keys: HashMap<Pubkey, Pubkey>,
    pub(crate) my_pubkey: Pubkey,
    pub(crate) leader_schedule_cache: Arc<LeaderScheduleCache>,
    poh_recorder: Arc<RwLock<PohRecorder>>,
    poh_controller: PohController,
    rpc_subscriptions: Arc<RpcSubscriptions>,
    pub vote_simulator: VoteSimulator,
}

pub fn replay_blockstore_components(
    forks: Option<Tree<Slot>>,
    num_validators: usize,
    generate_votes: Option<GenerateVotes>,
) -> ReplayBlockstoreComponents {
    // Setup blockstore
    let (vote_simulator, blockstore) = setup_forks_from_tree(
        forks.unwrap_or_else(|| tr(0)),
        num_validators,
        generate_votes,
    );

    let VoteSimulator {
        ref validator_keypairs,
        ref bank_forks,
        ..
    } = vote_simulator;

    let blockstore = Arc::new(blockstore);
    let validator_node_to_vote_keys: HashMap<Pubkey, Pubkey> = validator_keypairs
        .values()
        .map(|keypairs| {
            (
                keypairs.node_keypair.pubkey(),
                keypairs.vote_keypair.pubkey(),
            )
        })
        .collect();

    // ClusterInfo
    let my_keypairs = validator_keypairs.values().next().unwrap();
    let my_pubkey = my_keypairs.node_keypair.pubkey();
    let cluster_info = ClusterInfo::new(
        Node::new_localhost_with_pubkey(&my_pubkey).info,
        Arc::new(my_keypairs.node_keypair.insecure_clone()),
        SocketAddrSpace::Unspecified,
    );
    assert_eq!(my_pubkey, cluster_info.id());

    // Leader schedule cache
    let root_bank = bank_forks.read().unwrap().root_bank();
    let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&root_bank));

    let working_bank = bank_forks.read().unwrap().working_bank();
    let (_exit, poh_recorder, poh_controller, _transaction_recorder, _poh_service, _) =
        create_test_recorder(
            working_bank,
            blockstore.clone(),
            Some(PohConfig::default()),
            Some(leader_schedule_cache.clone()),
        );

    // RpcSubscriptions
    let optimistically_confirmed_bank =
        OptimisticallyConfirmedBank::locked_from_bank_forks_root(bank_forks);
    let exit = Arc::new(AtomicBool::new(false));
    let max_complete_transaction_status_slot = Arc::new(AtomicU64::default());
    let rpc_subscriptions = Arc::new(RpcSubscriptions::new_for_tests(
        exit,
        max_complete_transaction_status_slot,
        bank_forks.clone(),
        Arc::new(RwLock::new(BlockCommitmentCache::default())),
        optimistically_confirmed_bank,
    ));

    ReplayBlockstoreComponents {
        blockstore,
        validator_node_to_vote_keys,
        my_pubkey,
        leader_schedule_cache,
        poh_recorder,
        poh_controller,
        rpc_subscriptions,
        vote_simulator,
    }
}

#[test]
fn test_child_slots_of_same_parent() {
    let ReplayBlockstoreComponents {
        blockstore,
        validator_node_to_vote_keys,
        vote_simulator,
        leader_schedule_cache,
        rpc_subscriptions,
        ..
    } = replay_blockstore_components(None, 1, None::<GenerateVotes>);

    let VoteSimulator {
        mut progress,
        bank_forks,
        ..
    } = vote_simulator;

    // Insert a non-root bank so that the propagation logic will update this
    // bank
    let bank1 = Bank::new_from_parent(
        bank_forks.read().unwrap().get(0).unwrap(),
        leader_schedule_cache.slot_leader_at(1, None).unwrap(),
        1,
    );
    progress.insert(
        1,
        ForkProgress::new_from_bank(
            &bank1,
            bank1.leader_id(),
            validator_node_to_vote_keys.get(bank1.leader_id()).unwrap(),
            Some(0),
            0,
            0,
            None,
        ),
    );
    assert!(progress.get_propagated_stats(1).unwrap().is_leader_slot);
    bank1.freeze();
    bank_forks.write().unwrap().insert(bank1);

    let rpc_subscriptions = Some(rpc_subscriptions);

    // Insert shreds for slot NUM_CONSECUTIVE_LEADER_SLOTS,
    // chaining to slot 1
    let (shreds, _) = make_slot_entries(
        NUM_CONSECUTIVE_LEADER_SLOTS, // slot
        1,                            // parent_slot
        8,                            // num_entries
    );
    blockstore.insert_shreds(shreds, false).unwrap();
    assert!(
        bank_forks
            .read()
            .unwrap()
            .get(NUM_CONSECUTIVE_LEADER_SLOTS)
            .is_none()
    );
    let mut replay_timing = ReplayLoopTiming::default();
    ReplayStage::generate_new_bank_forks(
        NewBankForksContext {
            blockstore: &blockstore,
            bank_forks: &bank_forks,
            leader_schedule_cache: &leader_schedule_cache,
            rpc_subscriptions: rpc_subscriptions.as_deref(),
            slot_status_notifier: &None,
            transaction_status_sender: None,
            migration_status: &MigrationStatus::default(),
            my_pubkey: &Pubkey::default(),
        },
        &mut progress,
        &mut replay_timing,
    )
    .unwrap();
    assert!(
        bank_forks
            .read()
            .unwrap()
            .get(NUM_CONSECUTIVE_LEADER_SLOTS)
            .is_some()
    );

    // Insert shreds for slot 2 * NUM_CONSECUTIVE_LEADER_SLOTS,
    // chaining to slot 1
    let (shreds, _) = make_slot_entries(2 * NUM_CONSECUTIVE_LEADER_SLOTS, 1, 8);
    blockstore.insert_shreds(shreds, false).unwrap();
    assert!(
        bank_forks
            .read()
            .unwrap()
            .get(2 * NUM_CONSECUTIVE_LEADER_SLOTS)
            .is_none()
    );
    ReplayStage::generate_new_bank_forks(
        NewBankForksContext {
            blockstore: &blockstore,
            bank_forks: &bank_forks,
            leader_schedule_cache: &leader_schedule_cache,
            rpc_subscriptions: rpc_subscriptions.as_deref(),
            slot_status_notifier: &None,
            transaction_status_sender: None,
            migration_status: &MigrationStatus::default(),
            my_pubkey: &Pubkey::default(),
        },
        &mut progress,
        &mut replay_timing,
    )
    .unwrap();
    assert!(
        bank_forks
            .read()
            .unwrap()
            .get(NUM_CONSECUTIVE_LEADER_SLOTS)
            .is_some()
    );
    assert!(
        bank_forks
            .read()
            .unwrap()
            .get(2 * NUM_CONSECUTIVE_LEADER_SLOTS)
            .is_some()
    );

    // // There are 20 equally staked accounts, of which 3 have built
    // banks above or at bank 1. Because 3/20 < SUPERMINORITY_THRESHOLD,
    // we should see 3 validators in bank 1's propagated_validator set.
    let expected_leader_slots = vec![
        1,
        NUM_CONSECUTIVE_LEADER_SLOTS,
        2 * NUM_CONSECUTIVE_LEADER_SLOTS,
    ];
    for slot in expected_leader_slots {
        let leader = leader_schedule_cache.slot_leader_at(slot, None).unwrap();
        let vote_key = validator_node_to_vote_keys.get(&leader.id).unwrap();
        assert!(
            progress
                .get_propagated_stats(1)
                .unwrap()
                .propagated_validators
                .contains(vote_key)
        );
    }
}

#[test]
fn test_handle_new_root() {
    let genesis_config = create_genesis_config(10_000).genesis_config;
    let bank0 = Bank::new_for_tests(&genesis_config);
    let bank_forks = BankForks::new_rw_arc(bank0);

    let root = 3;
    let root_bank = Bank::new_from_parent(
        bank_forks.read().unwrap().get(0).unwrap(),
        SlotLeader::default(),
        root,
    );
    root_bank.freeze();
    let root_hash = root_bank.hash();
    bank_forks.write().unwrap().insert(root_bank);

    let heaviest_subtree_fork_choice = HeaviestSubtreeForkChoice::new((root, root_hash));

    let mut progress = ProgressMap::default();
    for i in 0..=root {
        progress.insert(
            i,
            ForkProgress::new(Hash::default(), None, None, 0, 0, None),
        );
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
    let (drop_bank_sender, _drop_bank_receiver) = bounded(1024);
    let mut tbft_structs = TowerBFTStructures {
        heaviest_subtree_fork_choice,
        duplicate_slots_tracker,
        duplicate_confirmed_slots,
        unfrozen_gossip_verified_vote_hashes,
        epoch_slots_frozen_slots,
    };
    ReplayStage::handle_new_root(
        &Pubkey::new_unique(),
        root,
        &bank_forks,
        &mut progress,
        None, // snapshot_controller
        None,
        &mut true,
        &mut Vec::new(),
        &drop_bank_sender,
        &mut tbft_structs,
    );
    assert_eq!(bank_forks.read().unwrap().root(), root);
    assert_eq!(progress.len(), 1);
    assert!(progress.get(&root).is_some());
    // root - 1 is filtered out
    assert_eq!(
        tbft_structs
            .duplicate_slots_tracker
            .into_iter()
            .collect::<Vec<Slot>>(),
        vec![root, root + 1]
    );
    assert_eq!(
        tbft_structs
            .duplicate_confirmed_slots
            .keys()
            .cloned()
            .collect::<Vec<Slot>>(),
        vec![root, root + 1]
    );
    assert_eq!(
        tbft_structs
            .unfrozen_gossip_verified_vote_hashes
            .votes_per_slot
            .keys()
            .cloned()
            .collect::<Vec<Slot>>(),
        vec![root, root + 1]
    );
    assert_eq!(
        tbft_structs
            .epoch_slots_frozen_slots
            .into_keys()
            .collect::<Vec<Slot>>(),
        vec![root, root + 1]
    );
}

#[test]
fn test_process_set_root_command_requires_matching_frozen_bank() {
    let (mut vote_simulator, blockstore) = setup_forks_from_tree(tr(0) / tr(1), 1, None);
    let bank_forks = vote_simulator.bank_forks.clone();
    let blockstore = Arc::new(blockstore);
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

    let missing_command = SetRootCommand {
        new_root: Block::new_unique(2),
    };
    ReplayStage::process_set_root_command(
        missing_command,
        &context,
        &my_pubkey,
        &mut vote_simulator.progress,
    );
    assert_eq!(bank_forks.read().unwrap().root(), 0);
    assert!(!blockstore.is_root(2));

    let mismatched_command = SetRootCommand {
        new_root: Block::new_unique(1),
    };
    ReplayStage::process_set_root_command(
        mismatched_command,
        &context,
        &my_pubkey,
        &mut vote_simulator.progress,
    );
    assert_eq!(bank_forks.read().unwrap().root(), 0);
    assert!(!blockstore.is_root(1));

    let unfrozen_block_id = BlockId::new_unique();
    let unfrozen_bank = Bank::new_from_parent(root_bank, SlotLeader::default(), 2);
    unfrozen_bank.set_block_id(Some(unfrozen_block_id));
    bank_forks.write().unwrap().insert(unfrozen_bank);
    let unfrozen_command = SetRootCommand {
        new_root: Block {
            slot: 2,
            block_id: unfrozen_block_id,
        },
    };
    ReplayStage::process_set_root_command(
        unfrozen_command,
        &context,
        &my_pubkey,
        &mut vote_simulator.progress,
    );
    assert_eq!(bank_forks.read().unwrap().root(), 0);
    assert!(!blockstore.is_root(2));
    // A root replay didn't apply is never published to the commitment cache.
    assert!(votor_commitment_receiver.is_empty());

    let block_id = bank_forks.read().unwrap().block_id(1).unwrap();
    let matching_command = SetRootCommand {
        new_root: Block { slot: 1, block_id },
    };
    ReplayStage::process_set_root_command(
        matching_command,
        &context,
        &my_pubkey,
        &mut vote_simulator.progress,
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
    assert!(votor_commitment_receiver.is_empty());
}

#[test]
fn test_handle_new_root_ahead_of_highest_super_majority_root() {
    let genesis_config = create_genesis_config(10_000).genesis_config;
    let bank0 = Bank::new_for_tests(&genesis_config);
    let bank_forks = BankForks::new_rw_arc(bank0);
    let confirmed_root = 1;
    let fork = 2;
    let bank1 = Bank::new_from_parent(
        bank_forks.read().unwrap().get(0).unwrap(),
        SlotLeader::default(),
        confirmed_root,
    );
    bank_forks.write().unwrap().insert(bank1);
    let bank2 = Bank::new_from_parent(
        bank_forks.read().unwrap().get(confirmed_root).unwrap(),
        SlotLeader::default(),
        fork,
    );
    bank_forks.write().unwrap().insert(bank2);
    let root = 3;
    let root_bank = Bank::new_from_parent(
        bank_forks.read().unwrap().get(confirmed_root).unwrap(),
        SlotLeader::default(),
        root,
    );
    root_bank.freeze();
    let root_hash = root_bank.hash();
    bank_forks.write().unwrap().insert(root_bank);
    let mut progress = ProgressMap::default();
    for i in 0..=root {
        progress.insert(
            i,
            ForkProgress::new(Hash::default(), None, None, 0, 0, None),
        );
    }
    let (drop_bank_sender, _drop_bank_receiver) = bounded(1024);
    let mut tbft_structs = TowerBFTStructures {
        heaviest_subtree_fork_choice: HeaviestSubtreeForkChoice::new((root, root_hash)),
        duplicate_slots_tracker: DuplicateSlotsTracker::default(),
        duplicate_confirmed_slots: DuplicateConfirmedSlots::default(),
        unfrozen_gossip_verified_vote_hashes: UnfrozenGossipVerifiedVoteHashes::default(),
        epoch_slots_frozen_slots: EpochSlotsFrozenSlots::default(),
    };
    ReplayStage::handle_new_root(
        &Pubkey::new_unique(),
        root,
        &bank_forks,
        &mut progress,
        None, // snapshot_controller
        Some(confirmed_root),
        &mut true,
        &mut Vec::new(),
        &drop_bank_sender,
        &mut tbft_structs,
    );
    assert_eq!(bank_forks.read().unwrap().root(), root);
    assert!(bank_forks.read().unwrap().get(confirmed_root).is_some());
    assert!(bank_forks.read().unwrap().get(fork).is_none());
    assert_eq!(progress.len(), 2);
    assert!(progress.get(&root).is_some());
    assert!(progress.get(&confirmed_root).is_some());
    assert!(progress.get(&fork).is_none());
}

fn make_complete_slot_entries(bank: &BankWithScheduler, txs: Vec<Transaction>) -> Vec<Entry> {
    let hashes_per_tick = bank.hashes_per_tick().unwrap();
    let tx_entry = entry::next_entry(&bank.last_blockhash(), hashes_per_tick - 1, txs);
    let first_tick = entry::next_entry(&tx_entry.hash, 1, vec![]);
    let prev_hash = first_tick.hash;
    let mut entries = vec![tx_entry, first_tick];
    entries.extend(entry::create_ticks(
        bank.ticks_per_slot() - 1,
        hashes_per_tick,
        prev_hash,
    ));
    entries
}

enum CompleteBankFailure {
    ReplayError,
    VerifyError,
}

fn do_test_dead_slot_on_complete_bank(failure: CompleteBankFailure) {
    let ReplayBlockstoreComponents {
        blockstore,
        vote_simulator,
        ..
    } = replay_blockstore_components(Some(tr(0)), 1, None);
    let funded_keypair = vote_simulator
        .validator_keypairs
        .values()
        .next()
        .unwrap()
        .node_keypair
        .insecure_clone();
    let bank_forks = vote_simulator.bank_forks;
    let mut progress = ProgressMap::default();
    let mut latest_validator_votes_for_frozen_banks = LatestValidatorVotesForFrozenBanks::default();

    let bank = bank_forks.read().unwrap().get(0).unwrap();
    bank_forks
        .write()
        .unwrap()
        .install_scheduler_pool(DefaultSchedulerPool::new_for_verification(
            None, None, None, None, None,
        ));

    let child_bank = Bank::new_from_parent(bank, SlotLeader::default(), 1);
    let bank = bank_forks.write().unwrap().insert(child_bank);

    let slot = bank.slot();
    let (replay_vote_sender, replay_vote_receiver) = bounded(match failure {
        CompleteBankFailure::ReplayError => 1024,
        CompleteBankFailure::VerifyError => 0,
    });
    let (finalization_cert_sender, _finalization_cert_receiver) = bounded(1024);
    let process_active_banks_context = ProcessActiveBanksContext::new_for_tests(
        bank_forks.clone(),
        blockstore.clone(),
        replay_vote_sender,
    );
    let replay_vote_message_thread = match failure {
        CompleteBankFailure::ReplayError => None,
        CompleteBankFailure::VerifyError => {
            let start = Arc::new(Barrier::new(2));
            let thread_start = Arc::clone(&start);
            let handle = std::thread::spawn(move || {
                // verification will block sending to replay_vote_sender. We use this to model
                // getting a verify error after replay has completed.
                thread_start.wait();
                // pop until the channel gets closed
                for _ in replay_vote_receiver {}
            });
            Some((start, handle))
        }
    };
    let replay_result = {
        let bank_progress = progress
            .entry(slot)
            .or_insert_with(|| ForkProgress::new(bank.last_blockhash(), None, None, 0, 0, None));
        let tx = match failure {
            CompleteBankFailure::ReplayError => {
                // trigger a replay error since blockhash is invalid
                system_transaction::transfer(
                    &funded_keypair,
                    &Keypair::new().pubkey(),
                    1,
                    Hash::new_unique(),
                )
            }
            CompleteBankFailure::VerifyError => {
                // trigger an invalid signature error
                let mut tx = system_transaction::transfer(
                    &funded_keypair,
                    &Keypair::new().pubkey(),
                    1,
                    bank.last_blockhash(),
                );
                tx.signatures[0] = Signature::default();
                tx
            }
        };
        let entries = make_complete_slot_entries(&bank, vec![tx]);
        let shreds = entries_to_test_shreds(&entries, slot, bank.parent_slot(), true, 0);
        blockstore.insert_shreds(shreds, false).unwrap();
        let cluster_info = cluster_info_for_tests();

        ReplaySlotFromBlockstore {
            is_slot_dead: false,
            bank_slot: slot,
            replay_result: Some(ReplayStage::replay_blockstore_into_bank(
                cluster_info.my_shred_version(),
                &process_active_banks_context,
                &bank,
                &bank_progress.replay_stats,
                &bank_progress.replay_progress,
                &finalization_cert_sender,
            )),
        }
    };

    // the sync path succeeded, we want to hit async failures
    assert_matches!(replay_result.replay_result, Some(Ok(1)));

    if let Some((start, _handle)) = &replay_vote_message_thread {
        start.wait();
    }

    let my_pubkey = Pubkey::default();

    assert!(!process_active_banks_context.blockstore.is_dead(slot));
    assert!(progress.get(&slot).unwrap().dead_reason.is_none());
    assert!(!blockstore.is_dead(slot));

    let mut duplicate_slots_to_repair = DuplicateSlotsToRepair::default();
    let mut purge_repair_slot_counter = PurgeRepairSlotCounter::default();
    ReplayStage::process_replay_results(
        &process_active_banks_context,
        &mut progress,
        &mut Vec::new(),
        &mut latest_validator_votes_for_frozen_banks,
        &mut duplicate_slots_to_repair,
        &mut purge_repair_slot_counter,
        None,
        &[replay_result],
        &my_pubkey,
    )
    .unwrap();

    if let Some((_, handle)) = replay_vote_message_thread {
        // drop the context so replay_vote_sender is dropped and the thread can exit
        drop(process_active_banks_context);
        handle.join().unwrap();
    }

    assert!(progress.get(&slot).unwrap().dead_reason.is_some());
    assert!(blockstore.is_dead(slot));
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
fn test_complete_bank_replay_sends_bank_complete() {
    let ReplayBlockstoreComponents {
        blockstore,
        vote_simulator,
        ..
    } = replay_blockstore_components(Some(tr(0)), 1, None);
    let bank_forks = vote_simulator.bank_forks;
    let bank = bank_forks.read().unwrap().get_with_scheduler(0).unwrap();
    let (replay_vote_sender, replay_vote_receiver) = bounded(1024);
    let process_active_banks_context = ProcessActiveBanksContext::new_for_tests(
        bank_forks.clone(),
        blockstore,
        replay_vote_sender,
    );
    let mut bank_progress = ForkProgress::new(
        bank.last_blockhash(),
        None,
        None,
        0,
        0,
        Some(AsyncVerificationProgress::new(1)),
    );
    let mut async_verification_freelist = Vec::new();

    let completed_replay = ReplayStage::complete_bank_replay(
        &process_active_banks_context,
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
    assert_eq!(
        replay_vote_receiver.try_recv(),
        Ok(ReplayVoteMessage::BankComplete {
            replay_bank_id: bank.bank_id(),
            replay_slot: bank.slot(),
        })
    );
    assert!(replay_vote_receiver.try_recv().is_err());
}

#[test]
fn test_cmr_mismatch_hard_dead() {
    let ReplayBlockstoreComponents {
        blockstore,
        vote_simulator,
        ..
    } = replay_blockstore_components(Some(tr(0) / tr(1)), 1, None::<GenerateVotes>);
    let VoteSimulator {
        bank_forks,
        mut progress,
        ..
    } = vote_simulator;

    let slot = 1;
    let bank = bank_forks.read().unwrap().get(slot).unwrap();
    progress.insert(
        slot,
        ForkProgress::new(bank.last_blockhash(), Some(0), None, 0, 0, None),
    );

    let (replay_vote_sender, replay_vote_receiver) = bounded(1024);
    let process_active_banks_context = ProcessActiveBanksContext::new_for_tests(
        bank_forks.clone(),
        blockstore.clone(),
        replay_vote_sender,
    );
    let replay_result = ReplaySlotFromBlockstore {
        is_slot_dead: false,
        bank_slot: slot,
        replay_result: Some(Err(BlockstoreProcessorError::ChainedBlockIdFailure(
            slot, 0,
        ))),
    };

    ReplayStage::process_replay_results(
        &process_active_banks_context,
        &mut progress,
        &mut Vec::new(),
        &mut LatestValidatorVotesForFrozenBanks::default(),
        &mut DuplicateSlotsToRepair::default(),
        &mut PurgeRepairSlotCounter::default(),
        None,
        &[replay_result],
        &Pubkey::new_unique(),
    )
    .unwrap();

    assert!(blockstore.is_dead(slot));
    assert_eq!(progress.dead_reason(slot), Some(&DeadSlotReason::Hard));
    assert_eq!(
        replay_vote_receiver.try_recv(),
        Ok(ReplayVoteMessage::InvalidBank {
            replay_bank_id: bank.bank_id(),
            replay_slot: slot,
        })
    );
}

#[test]
fn test_alpenglow_migration_transition_does_not_mark_bank_dead() {
    let ReplayBlockstoreComponents {
        blockstore,
        vote_simulator,
        ..
    } = replay_blockstore_components(Some(tr(0) / tr(1)), 1, None::<GenerateVotes>);
    let VoteSimulator {
        bank_forks,
        mut progress,
        ..
    } = vote_simulator;

    let slot = 1;
    let bank = bank_forks.read().unwrap().get(slot).unwrap();
    progress.insert(
        slot,
        ForkProgress::new(bank.last_blockhash(), Some(0), None, 0, 0, None),
    );

    let (replay_vote_sender, replay_vote_receiver) = bounded(1024);
    let process_active_banks_context = ProcessActiveBanksContext::new_for_tests(
        bank_forks.clone(),
        blockstore.clone(),
        replay_vote_sender,
    );
    let genesis_block = Block::new_unique(0);
    process_active_banks_context
        .migration_status
        .record_feature_activation(0);
    process_active_banks_context
        .migration_status
        .set_genesis_block(genesis_block);
    process_active_banks_context
        .migration_status
        .set_genesis_certificate(Arc::new(GenesisCert {
            block: genesis_block,
            signature: CertSignature {
                signature: BLSSignature([0; BLS_SIGNATURE_AFFINE_SIZE]),
                bitmap: vec![],
            },
        }));
    assert!(
        process_active_banks_context
            .migration_status
            .is_ready_to_enable()
    );

    let replay_result = ReplaySlotFromBlockstore {
        is_slot_dead: false,
        bank_slot: slot,
        replay_result: Some(Err(BlockstoreProcessorError::BlockComponentProcessor(
            BlockComponentProcessorError::AlpenglowMigrationTransition,
        ))),
    };

    ReplayStage::process_replay_results(
        &process_active_banks_context,
        &mut progress,
        &mut Vec::new(),
        &mut LatestValidatorVotesForFrozenBanks::default(),
        &mut DuplicateSlotsToRepair::default(),
        &mut PurgeRepairSlotCounter::default(),
        None,
        &[replay_result],
        &Pubkey::new_unique(),
    )
    .unwrap();

    assert!(!blockstore.is_dead(slot));
    assert!(progress.get(&slot).unwrap().dead_reason.is_none());
    assert!(bank_forks.read().unwrap().get(slot).is_some());
    assert!(replay_vote_receiver.try_recv().is_err());
}

#[test]
fn test_abandon_invalidates() {
    let ReplayBlockstoreComponents {
        blockstore,
        vote_simulator,
        ..
    } = replay_blockstore_components(Some(tr(0) / tr(1)), 1, None::<GenerateVotes>);
    let VoteSimulator {
        bank_forks,
        mut progress,
        ..
    } = vote_simulator;

    let slot = 4;
    let parent_block_id = BlockId::new_unique();
    insert_update_parent_slot(&blockstore, slot, 3, 0, parent_block_id, 32);
    let bank0 = bank_forks.read().unwrap().get(0).unwrap();
    let bank = Bank::new_from_parent(bank0, SlotLeader::default(), slot);
    bank_forks.write().unwrap().insert(bank);
    let bank = bank_forks.read().unwrap().get(slot).unwrap();
    progress.insert(
        slot,
        ForkProgress::new(bank.last_blockhash(), Some(0), None, 0, 0, None),
    );

    let (replay_vote_sender, replay_vote_receiver) = bounded(1024);
    let mut process_active_banks_context = ProcessActiveBanksContext::new_for_tests(
        bank_forks.clone(),
        blockstore,
        replay_vote_sender,
    );
    let (entry_notification_sender, entry_notification_receiver) = bounded(1);
    process_active_banks_context.entry_notification_sender = Some(entry_notification_sender);
    let (transaction_status_sender, purge_response_thread) = transaction_history_purge_responder();
    process_active_banks_context.transaction_status_sender = Some(transaction_status_sender);
    let update_parent = VersionedUpdateParent::V1(solana_entry::block_component::UpdateParentV1 {
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
        &process_active_banks_context,
        &mut progress,
        &mut Vec::new(),
        &mut LatestValidatorVotesForFrozenBanks::default(),
        &mut DuplicateSlotsToRepair::default(),
        &mut PurgeRepairSlotCounter::default(),
        None,
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
    assert_eq!(
        replay_vote_receiver.try_recv(),
        Ok(ReplayVoteMessage::InvalidBank {
            replay_bank_id: bank.bank_id(),
            replay_slot: slot,
        })
    );
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
fn test_replay_commitment_cache() {
    fn leader_vote(vote_slot: Slot, bank: &Bank, pubkey: &Pubkey) -> (Pubkey, TowerVoteState) {
        let mut leader_vote_account = bank.get_account(pubkey).unwrap();
        let mut vote_state = VoteStateHandler::new_v4(
            VoteStateV4::deserialize(leader_vote_account.data(), pubkey).unwrap(),
        );
        vote_state::process_slot_vote_unchecked(&mut vote_state, vote_slot);
        let vote_state = vote_state.unwrap_v4();
        let versioned = VoteStateVersions::new_v4(vote_state.clone());
        leader_vote_account.set_state(&versioned).unwrap();
        bank.store_account(pubkey, &leader_vote_account);
        (*pubkey, TowerVoteState::from(vote_state))
    }

    let leader_pubkey = solana_pubkey::new_rand();
    let leader_lamports = 3;
    let genesis_config_info =
        create_genesis_config_with_leader(50, &leader_pubkey, leader_lamports);
    let mut genesis_config = genesis_config_info.genesis_config;
    let leader_voting_pubkey = genesis_config_info.voting_keypair.pubkey();
    genesis_config.epoch_schedule.warmup = false;
    genesis_config.ticks_per_slot = 4;
    let bank0 = Bank::new_for_tests(&genesis_config);
    for _ in 0..genesis_config.ticks_per_slot {
        bank0.register_default_tick_for_test();
    }
    bank0.freeze();
    let bank_forks = BankForks::new_rw_arc(bank0);

    let exit = Arc::new(AtomicBool::new(false));
    let block_commitment_cache = Arc::new(RwLock::new(BlockCommitmentCache::default()));
    let max_complete_transaction_status_slot = Arc::new(AtomicU64::default());
    let rpc_subscriptions = Some(Arc::new(RpcSubscriptions::new_for_tests(
        exit.clone(),
        max_complete_transaction_status_slot,
        bank_forks.clone(),
        block_commitment_cache.clone(),
        OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks),
    )));
    let (lockouts_sender, _alpenglow_sender, _commitment_service) =
        AggregateCommitmentService::new(exit, block_commitment_cache.clone(), rpc_subscriptions);

    assert!(
        block_commitment_cache
            .read()
            .unwrap()
            .get_block_commitment(0)
            .is_none()
    );
    assert!(
        block_commitment_cache
            .read()
            .unwrap()
            .get_block_commitment(1)
            .is_none()
    );

    for i in 1..=3 {
        let prev_bank = bank_forks.read().unwrap().get(i - 1).unwrap();
        let slot = prev_bank.slot() + 1;
        let bank = Bank::new_from_parent_with_bank_forks(
            bank_forks.as_ref(),
            prev_bank,
            SlotLeader::default(),
            slot,
        );
        let _res = bank.transfer(
            10,
            &genesis_config_info.mint_keypair,
            &solana_pubkey::new_rand(),
        );
        for _ in 0..genesis_config.ticks_per_slot {
            bank.register_default_tick_for_test();
        }

        let arc_bank = bank_forks.read().unwrap().get(i).unwrap();
        let node_vote_state = leader_vote(i - 1, &arc_bank, &leader_voting_pubkey);
        ReplayStage::update_commitment_cache(
            arc_bank.clone(),
            0,
            leader_lamports,
            node_vote_state,
            &lockouts_sender,
        );
        arc_bank.freeze();
    }

    for _ in 0..10 {
        let done = {
            let bcc = block_commitment_cache.read().unwrap();
            bcc.get_block_commitment(0).is_some()
                && bcc.get_block_commitment(1).is_some()
                && bcc.get_block_commitment(2).is_some()
        };
        if done {
            break;
        } else {
            thread::sleep(Duration::from_millis(200));
        }
    }

    let mut expected0 = BlockCommitment::default();
    expected0.increase_confirmation_stake(3, leader_lamports);
    assert_eq!(
        block_commitment_cache
            .read()
            .unwrap()
            .get_block_commitment(0)
            .unwrap(),
        &expected0,
    );
    let mut expected1 = BlockCommitment::default();
    expected1.increase_confirmation_stake(2, leader_lamports);
    assert_eq!(
        block_commitment_cache
            .read()
            .unwrap()
            .get_block_commitment(1)
            .unwrap(),
        &expected1
    );
    let mut expected2 = BlockCommitment::default();
    expected2.increase_confirmation_stake(1, leader_lamports);
    assert_eq!(
        block_commitment_cache
            .read()
            .unwrap()
            .get_block_commitment(2)
            .unwrap(),
        &expected2
    );
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
fn test_same_weight_select_lower_slot() {
    // Init state
    let mut vote_simulator = VoteSimulator::new(1);
    let mut tower = Tower::default();

    // Create the tree of banks in a BankForks object
    let forks = tr(0) / (tr(1)) / (tr(2));
    vote_simulator.fill_bank_forks(forks, &HashMap::new(), true);
    let mut frozen_banks: Vec<_> = vote_simulator
        .bank_forks
        .read()
        .unwrap()
        .frozen_banks()
        .map(|(_slot, bank)| bank)
        .collect();
    let heaviest_subtree_fork_choice =
        &mut vote_simulator.tbft_structs.heaviest_subtree_fork_choice;
    let mut latest_validator_votes_for_frozen_banks = LatestValidatorVotesForFrozenBanks::default();
    let ancestors = vote_simulator.bank_forks.read().unwrap().ancestors();
    let mut vote_slots = HashSet::default();

    let my_vote_pubkey = vote_simulator.vote_pubkeys[0];
    ReplayStage::compute_bank_stats(
        &my_vote_pubkey,
        &ancestors,
        &mut frozen_banks,
        &mut tower,
        &mut vote_simulator.progress,
        &VoteTracker::default(),
        &ClusterSlots::default_for_tests(),
        &vote_simulator.bank_forks,
        heaviest_subtree_fork_choice,
        &mut latest_validator_votes_for_frozen_banks,
        &mut vote_slots,
        &MigrationStatus::default(),
    );

    let bank1 = vote_simulator.bank_forks.read().unwrap().get(1).unwrap();
    let bank2 = vote_simulator.bank_forks.read().unwrap().get(2).unwrap();
    assert_eq!(
        heaviest_subtree_fork_choice
            .stake_voted_subtree(&(1, bank1.hash()))
            .unwrap(),
        heaviest_subtree_fork_choice
            .stake_voted_subtree(&(2, bank2.hash()))
            .unwrap()
    );

    let (heaviest_bank, _) = heaviest_subtree_fork_choice.select_forks(
        &frozen_banks,
        &tower,
        &vote_simulator.progress,
        &ancestors,
        &vote_simulator.bank_forks,
    );

    // Should pick the lower of the two equally weighted banks
    assert_eq!(heaviest_bank.slot(), 1);
}

#[test]
fn test_should_retransmit() {
    let poh_slot = 4;
    let mut last_retransmit_slot = 4;
    // We retransmitted already at slot 4, shouldn't retransmit until
    // >= 4 + NUM_CONSECUTIVE_LEADER_SLOTS, or if we reset to < 4
    assert!(!ReplayStage::should_retransmit(
        poh_slot,
        &mut last_retransmit_slot
    ));
    assert_eq!(last_retransmit_slot, 4);

    for poh_slot in 4..4 + NUM_CONSECUTIVE_LEADER_SLOTS {
        assert!(!ReplayStage::should_retransmit(
            poh_slot,
            &mut last_retransmit_slot
        ));
        assert_eq!(last_retransmit_slot, 4);
    }

    let poh_slot = 4 + NUM_CONSECUTIVE_LEADER_SLOTS;
    last_retransmit_slot = 4;
    assert!(ReplayStage::should_retransmit(
        poh_slot,
        &mut last_retransmit_slot
    ));
    assert_eq!(last_retransmit_slot, poh_slot);

    let poh_slot = 3;
    last_retransmit_slot = 4;
    assert!(ReplayStage::should_retransmit(
        poh_slot,
        &mut last_retransmit_slot
    ));
    assert_eq!(last_retransmit_slot, poh_slot);
}

#[test]
fn test_update_slot_propagated_threshold_from_votes() {
    let keypairs: HashMap<_, _> = iter::repeat_with(|| {
        let vote_keypairs = ValidatorVoteKeypairs::new_rand();
        (vote_keypairs.node_keypair.pubkey(), vote_keypairs)
    })
    .take(10)
    .collect();

    let new_vote_pubkeys: Vec<_> = keypairs
        .values()
        .map(|keys| keys.vote_keypair.pubkey())
        .collect();
    let new_node_pubkeys: Vec<_> = keypairs
        .values()
        .map(|keys| keys.node_keypair.pubkey())
        .collect();

    // Once 4/10 validators have voted, we have hit threshold
    run_test_update_slot_propagated_threshold_from_votes(&keypairs, &new_vote_pubkeys, &[], 4);
    // Adding the same node pubkey's instead of the corresponding
    // vote pubkeys should be equivalent
    run_test_update_slot_propagated_threshold_from_votes(&keypairs, &[], &new_node_pubkeys, 4);
    // Adding the same node pubkey's in the same order as their
    // corresponding vote accounts is redundant, so we don't
    // reach the threshold any sooner.
    run_test_update_slot_propagated_threshold_from_votes(
        &keypairs,
        &new_vote_pubkeys,
        &new_node_pubkeys,
        4,
    );
    // However, if we add different node pubkey's than the
    // vote accounts, we should hit threshold much faster
    // because now we are getting 2 new pubkeys on each
    // iteration instead of 1, so by the 2nd iteration
    // we should have 4/10 validators voting
    run_test_update_slot_propagated_threshold_from_votes(
        &keypairs,
        &new_vote_pubkeys[0..5],
        &new_node_pubkeys[5..],
        2,
    );
}

fn run_test_update_slot_propagated_threshold_from_votes(
    all_keypairs: &HashMap<Pubkey, ValidatorVoteKeypairs>,
    new_vote_pubkeys: &[Pubkey],
    new_node_pubkeys: &[Pubkey],
    success_index: usize,
) {
    let stake = 10_000;
    let (bank_forks, _, _) = vote_simulator::initialize_state(all_keypairs, stake);
    let root_bank = bank_forks.read().unwrap().root_bank();
    let mut propagated_stats = PropagatedStats {
        total_epoch_stake: stake * all_keypairs.len() as u64,
        ..PropagatedStats::default()
    };

    let child_reached_threshold = false;
    for i in 0..std::cmp::max(new_vote_pubkeys.len(), new_node_pubkeys.len()) {
        propagated_stats.is_propagated = false;
        let len = std::cmp::min(i, new_vote_pubkeys.len());
        let mut voted_pubkeys = new_vote_pubkeys[..len].to_vec();
        let len = std::cmp::min(i, new_node_pubkeys.len());
        let mut node_pubkeys = new_node_pubkeys[..len].to_vec();
        let did_newly_reach_threshold = ReplayStage::update_slot_propagated_threshold_from_votes(
            &mut voted_pubkeys,
            &mut node_pubkeys,
            &root_bank,
            &mut propagated_stats,
            child_reached_threshold,
        );

        // Only the i'th voted pubkey should be new (everything else was
        // inserted in previous iteration of the loop), so those redundant
        // pubkeys should have been filtered out
        let remaining_vote_pubkeys = {
            if i == 0 || i >= new_vote_pubkeys.len() {
                vec![]
            } else {
                vec![new_vote_pubkeys[i - 1]]
            }
        };
        let remaining_node_pubkeys = {
            if i == 0 || i >= new_node_pubkeys.len() {
                vec![]
            } else {
                vec![new_node_pubkeys[i - 1]]
            }
        };
        assert_eq!(voted_pubkeys, remaining_vote_pubkeys);
        assert_eq!(node_pubkeys, remaining_node_pubkeys);

        // If we crossed the superminority threshold, then
        // `did_newly_reach_threshold == true`, otherwise the
        // threshold has not been reached
        if i >= success_index {
            assert!(propagated_stats.is_propagated);
            assert!(did_newly_reach_threshold);
        } else {
            assert!(!propagated_stats.is_propagated);
            assert!(!did_newly_reach_threshold);
        }
    }
}

#[test]
fn test_update_slot_propagated_threshold_from_votes2() {
    let mut empty: Vec<Pubkey> = vec![];
    let genesis_config = create_genesis_config(100_000_000).genesis_config;
    let root_bank = Bank::new_for_tests(&genesis_config);
    let stake = 10_000;
    // Simulate a child slot seeing threshold (`child_reached_threshold` = true),
    // then the parent should also be marked as having reached threshold,
    // even if there are no new pubkeys to add (`newly_voted_pubkeys.is_empty()`)
    let mut propagated_stats = PropagatedStats {
        total_epoch_stake: stake * 10,
        ..PropagatedStats::default()
    };
    propagated_stats.total_epoch_stake = stake * 10;
    let child_reached_threshold = true;
    let mut newly_voted_pubkeys: Vec<Pubkey> = vec![];

    assert!(ReplayStage::update_slot_propagated_threshold_from_votes(
        &mut newly_voted_pubkeys,
        &mut empty,
        &root_bank,
        &mut propagated_stats,
        child_reached_threshold,
    ));

    // If propagation already happened (propagated_stats.is_propagated = true),
    // always returns false
    propagated_stats = PropagatedStats {
        total_epoch_stake: stake * 10,
        ..PropagatedStats::default()
    };
    propagated_stats.is_propagated = true;
    newly_voted_pubkeys = vec![];
    assert!(!ReplayStage::update_slot_propagated_threshold_from_votes(
        &mut newly_voted_pubkeys,
        &mut empty,
        &root_bank,
        &mut propagated_stats,
        child_reached_threshold,
    ));

    let child_reached_threshold = false;
    assert!(!ReplayStage::update_slot_propagated_threshold_from_votes(
        &mut newly_voted_pubkeys,
        &mut empty,
        &root_bank,
        &mut propagated_stats,
        child_reached_threshold,
    ));
}

#[test]
fn test_update_propagation_status() {
    // Create genesis stakers
    let vote_keypairs = ValidatorVoteKeypairs::new_rand();
    let node_pubkey = vote_keypairs.node_keypair.pubkey();
    let vote_pubkey = vote_keypairs.vote_keypair.pubkey();
    let keypairs: HashMap<_, _> = vec![(node_pubkey, vote_keypairs)].into_iter().collect();
    let stake = 10_000;
    let (bank_forks_arc, mut progress_map, _) = vote_simulator::initialize_state(&keypairs, stake);

    let bank0 = bank_forks_arc.read().unwrap().get(0).unwrap();
    Bank::new_from_parent_with_bank_forks(&bank_forks_arc, bank0.clone(), SlotLeader::default(), 9);
    let bank9 = bank_forks_arc.read().unwrap().get(9).unwrap();
    Bank::new_from_parent_with_bank_forks(&bank_forks_arc, bank9, SlotLeader::default(), 10);
    bank_forks_arc.write().unwrap().set_root(9, None, None);
    let total_epoch_stake = bank0.total_epoch_stake();

    // Insert new ForkProgress for slot 10 and its
    // previous leader slot 9
    progress_map.insert(
        10,
        ForkProgress::new(
            Hash::default(),
            Some(9),
            Some(ValidatorStakeInfo {
                total_epoch_stake,
                ..ValidatorStakeInfo::default()
            }),
            0,
            0,
            None,
        ),
    );
    progress_map.insert(
        9,
        ForkProgress::new(
            Hash::default(),
            Some(8),
            Some(ValidatorStakeInfo {
                total_epoch_stake,
                ..ValidatorStakeInfo::default()
            }),
            0,
            0,
            None,
        ),
    );

    // Make sure is_propagated == false so that the propagation logic
    // runs in `update_propagation_status`
    assert!(!progress_map.get_leader_propagation_slot_must_exist(10).0);

    let vote_tracker = VoteTracker::default();
    vote_tracker.insert_vote(10, vote_pubkey);
    ReplayStage::update_propagation_status(
        &mut progress_map,
        10,
        &bank_forks_arc,
        &vote_tracker,
        &ClusterSlots::default_for_tests(),
    );

    let propagated_stats = &progress_map.get(&10).unwrap().propagated_stats;

    // There should now be a cached reference to the VoteTracker for
    // slot 10
    assert!(propagated_stats.slot_vote_tracker.is_some());

    // Updates should have been consumed
    assert!(
        propagated_stats
            .slot_vote_tracker
            .as_ref()
            .unwrap()
            .write()
            .unwrap()
            .get_voted_slot_updates()
            .is_none()
    );

    // The voter should be recorded
    assert!(
        propagated_stats
            .propagated_validators
            .contains(&vote_pubkey)
    );

    assert_eq!(propagated_stats.propagated_validators_stake, stake);
}

#[test]
fn test_chain_update_propagation_status() {
    let keypairs: HashMap<_, _> = iter::repeat_with(|| {
        let vote_keypairs = ValidatorVoteKeypairs::new_rand();
        (vote_keypairs.node_keypair.pubkey(), vote_keypairs)
    })
    .take(10)
    .collect();

    let vote_pubkeys: Vec<_> = keypairs
        .values()
        .map(|keys| keys.vote_keypair.pubkey())
        .collect();

    let stake_per_validator = 10_000;
    let (bank_forks_arc, mut progress_map, _) =
        vote_simulator::initialize_state(&keypairs, stake_per_validator);
    progress_map
        .get_propagated_stats_mut(0)
        .unwrap()
        .is_leader_slot = true;
    bank_forks_arc.write().unwrap().set_root(0, None, None);
    let total_epoch_stake = bank_forks_arc
        .read()
        .unwrap()
        .root_bank()
        .total_epoch_stake();

    // Insert new ForkProgress representing a slot for all slots 1..=num_banks. Only
    // make even numbered ones leader slots
    for i in 1..=10 {
        let parent_bank = bank_forks_arc.read().unwrap().get(i - 1).unwrap().clone();
        let prev_leader_slot = ((i - 1) / 2) * 2;
        let bank = Bank::new_from_parent(parent_bank, SlotLeader::default(), i);
        bank_forks_arc.write().unwrap().insert(bank);
        progress_map.insert(
            i,
            ForkProgress::new(
                Hash::default(),
                Some(prev_leader_slot),
                {
                    if i % 2 == 0 {
                        Some(ValidatorStakeInfo {
                            total_epoch_stake,
                            ..ValidatorStakeInfo::default()
                        })
                    } else {
                        None
                    }
                },
                0,
                0,
                None,
            ),
        );
    }

    let vote_tracker = VoteTracker::default();
    for vote_pubkey in &vote_pubkeys {
        // Insert a vote for the last bank for each voter
        vote_tracker.insert_vote(10, *vote_pubkey);
    }

    // The last bank should reach propagation threshold, and propagate it all
    // the way back through earlier leader banks
    ReplayStage::update_propagation_status(
        &mut progress_map,
        10,
        &bank_forks_arc,
        &vote_tracker,
        &ClusterSlots::default_for_tests(),
    );

    for i in 1..=10 {
        let propagated_stats = &progress_map.get(&i).unwrap().propagated_stats;
        // Only the even numbered ones were leader banks, so only
        // those should have been updated
        if i % 2 == 0 {
            assert!(propagated_stats.is_propagated);
        } else {
            assert!(!propagated_stats.is_propagated);
        }
    }
}

#[test]
fn test_chain_update_propagation_status2() {
    let num_validators = 6;
    let keypairs: HashMap<_, _> = iter::repeat_with(|| {
        let vote_keypairs = ValidatorVoteKeypairs::new_rand();
        (vote_keypairs.node_keypair.pubkey(), vote_keypairs)
    })
    .take(num_validators)
    .collect();

    let vote_pubkeys: Vec<_> = keypairs
        .values()
        .map(|keys| keys.vote_keypair.pubkey())
        .collect();

    let stake_per_validator = 10_000;
    let (bank_forks_arc, mut progress_map, _) =
        vote_simulator::initialize_state(&keypairs, stake_per_validator);
    progress_map
        .get_propagated_stats_mut(0)
        .unwrap()
        .is_leader_slot = true;
    bank_forks_arc.write().unwrap().set_root(0, None, None);

    let total_epoch_stake = num_validators as u64 * stake_per_validator;

    // Insert new ForkProgress representing a slot for all slots 1..=num_banks. Only
    // make even numbered ones leader slots
    for i in 1..=10 {
        let parent_bank = bank_forks_arc.read().unwrap().get(i - 1).unwrap().clone();
        let prev_leader_slot = i - 1;
        let bank = Bank::new_from_parent(parent_bank, SlotLeader::default(), i);
        bank_forks_arc.write().unwrap().insert(bank);
        let mut fork_progress = ForkProgress::new(
            Hash::default(),
            Some(prev_leader_slot),
            Some(ValidatorStakeInfo {
                total_epoch_stake,
                ..ValidatorStakeInfo::default()
            }),
            0,
            0,
            None,
        );

        let end_range = {
            // The earlier slots are one pubkey away from reaching confirmation
            if i < 5 {
                2
            } else {
                // The later slots are two pubkeys away from reaching confirmation
                1
            }
        };
        fork_progress.propagated_stats.propagated_validators =
            vote_pubkeys[0..end_range].iter().copied().collect();
        fork_progress.propagated_stats.propagated_validators_stake =
            end_range as u64 * stake_per_validator;
        progress_map.insert(i, fork_progress);
    }

    let vote_tracker = VoteTracker::default();
    // Insert a new vote
    vote_tracker.insert_vote(10, vote_pubkeys[2]);

    // The last bank should reach propagation threshold, and propagate it all
    // the way back through earlier leader banks
    ReplayStage::update_propagation_status(
        &mut progress_map,
        10,
        &bank_forks_arc,
        &vote_tracker,
        &ClusterSlots::default_for_tests(),
    );

    // Only the first 5 banks should have reached the threshold
    for i in 1..=10 {
        let propagated_stats = &progress_map.get(&i).unwrap().propagated_stats;
        if i < 5 {
            assert!(propagated_stats.is_propagated);
        } else {
            assert!(!propagated_stats.is_propagated);
        }
    }
}

#[test]
fn test_check_propagation_for_start_leader() {
    let mut progress_map = ProgressMap::default();
    let poh_slot = 5;
    let parent_slot = poh_slot - NUM_CONSECUTIVE_LEADER_SLOTS;

    // If there is no previous leader slot (previous leader slot is None),
    // should succeed
    progress_map.insert(
        parent_slot,
        ForkProgress::new(Hash::default(), None, None, 0, 0, None),
    );
    assert!(ReplayStage::check_propagation_for_start_leader(
        poh_slot,
        parent_slot,
        &progress_map,
    ));

    // Now if we make the parent was itself the leader, then requires propagation
    // confirmation check because the parent is at least NUM_CONSECUTIVE_LEADER_SLOTS
    // slots from the `poh_slot`
    progress_map.insert(
        parent_slot,
        ForkProgress::new(
            Hash::default(),
            None,
            Some(ValidatorStakeInfo::default()),
            0,
            0,
            None,
        ),
    );
    assert!(!ReplayStage::check_propagation_for_start_leader(
        poh_slot,
        parent_slot,
        &progress_map,
    ));
    progress_map
        .get_mut(&parent_slot)
        .unwrap()
        .propagated_stats
        .is_propagated = true;
    assert!(ReplayStage::check_propagation_for_start_leader(
        poh_slot,
        parent_slot,
        &progress_map,
    ));
    // Now, set up the progress map to show that the `previous_leader_slot` of 5 is
    // `parent_slot - 1` (not equal to the actual parent!), so `parent_slot - 1` needs
    // to see propagation confirmation before we can start a leader for block 5
    let previous_leader_slot = parent_slot - 1;
    progress_map.insert(
        parent_slot,
        ForkProgress::new(
            Hash::default(),
            Some(previous_leader_slot),
            None,
            0,
            0,
            None,
        ),
    );
    progress_map.insert(
        previous_leader_slot,
        ForkProgress::new(
            Hash::default(),
            None,
            Some(ValidatorStakeInfo::default()),
            0,
            0,
            None,
        ),
    );

    // `previous_leader_slot` has not seen propagation threshold, so should fail
    assert!(!ReplayStage::check_propagation_for_start_leader(
        poh_slot,
        parent_slot,
        &progress_map,
    ));

    // If we set the is_propagated = true for the `previous_leader_slot`, should
    // allow the block to be generated
    progress_map
        .get_mut(&previous_leader_slot)
        .unwrap()
        .propagated_stats
        .is_propagated = true;
    assert!(ReplayStage::check_propagation_for_start_leader(
        poh_slot,
        parent_slot,
        &progress_map,
    ));

    // If the root is now set to `parent_slot`, this filters out `previous_leader_slot` from the progress map,
    // which implies confirmation
    let genesis_config = genesis_config::create_genesis_config(10000).0;
    let (bank0, bank_forks_arc) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let parent_slot_bank =
        Bank::new_from_parent(Arc::clone(&bank0), SlotLeader::default(), parent_slot);
    bank_forks_arc.write().unwrap().insert(parent_slot_bank);
    let parent_bank = bank_forks_arc.read().unwrap().get(parent_slot).unwrap();
    let bank5 = Bank::new_from_parent(parent_bank, SlotLeader::default(), 5);
    bank_forks_arc.write().unwrap().insert(bank5);

    // Should purge only `previous_leader_slot` from the progress map
    progress_map.handle_new_root(&bank_forks_arc.read().unwrap());

    // Should succeed
    assert!(ReplayStage::check_propagation_for_start_leader(
        poh_slot,
        parent_slot,
        &progress_map,
    ));
}

#[test]
fn test_check_propagation_skip_propagation_check() {
    let mut progress_map = ProgressMap::default();
    let poh_slot = 4;
    let mut parent_slot = poh_slot - 1;

    // Set up the progress map to show that the last leader slot of 4 is 3,
    // which means 3 and 4 are consecutive leader slots
    progress_map.insert(
        3,
        ForkProgress::new(
            Hash::default(),
            None,
            Some(ValidatorStakeInfo::default()),
            0,
            0,
            None,
        ),
    );

    // If the previous leader slot has not seen propagation threshold, but
    // was the direct parent (implying consecutive leader slots), create
    // the block regardless
    assert!(ReplayStage::check_propagation_for_start_leader(
        poh_slot,
        parent_slot,
        &progress_map,
    ));

    // If propagation threshold was achieved on parent, block should
    // also be created
    progress_map
        .get_mut(&3)
        .unwrap()
        .propagated_stats
        .is_propagated = true;
    assert!(ReplayStage::check_propagation_for_start_leader(
        poh_slot,
        parent_slot,
        &progress_map,
    ));

    // Now insert another parent slot 2 for which this validator is also the leader
    parent_slot = poh_slot - NUM_CONSECUTIVE_LEADER_SLOTS + 1;
    progress_map.insert(
        parent_slot,
        ForkProgress::new(
            Hash::default(),
            None,
            Some(ValidatorStakeInfo::default()),
            0,
            0,
            None,
        ),
    );

    // Even though `parent_slot` and `poh_slot` are separated by another block,
    // because they're within `NUM_CONSECUTIVE` blocks of each other, the propagation
    // check is still skipped
    assert!(ReplayStage::check_propagation_for_start_leader(
        poh_slot,
        parent_slot,
        &progress_map,
    ));

    // Once the distance becomes >= NUM_CONSECUTIVE_LEADER_SLOTS, then we need to
    // enforce the propagation check
    parent_slot = poh_slot - NUM_CONSECUTIVE_LEADER_SLOTS;
    progress_map.insert(
        parent_slot,
        ForkProgress::new(
            Hash::default(),
            None,
            Some(ValidatorStakeInfo::default()),
            0,
            0,
            None,
        ),
    );
    assert!(!ReplayStage::check_propagation_for_start_leader(
        poh_slot,
        parent_slot,
        &progress_map,
    ));
}

#[test]
fn test_clear_slots_clears_status_cache_for_removed_bank() {
    let VoteSimulator {
        bank_forks,
        node_pubkeys,
        validator_keypairs,
        mut progress,
        ..
    } = VoteSimulator::new(1);
    let bank0 = bank_forks.read().unwrap().get(0).unwrap();
    let bank1 = Bank::new_from_parent(bank0, SlotLeader::default(), 1);
    let sender = node_pubkeys[0];
    let transfer_signature = bank1
        .transfer(
            1,
            &validator_keypairs.get(&sender).unwrap().node_keypair,
            &Pubkey::new_unique(),
        )
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
    let VoteSimulator {
        bank_forks,
        mut progress,
        ..
    } = VoteSimulator::new(1);
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
    let (vote_simulator, blockstore) =
        setup_forks_from_tree(tr(0) / tr(1), 1, None::<GenerateVotes>);
    let VoteSimulator {
        bank_forks,
        mut progress,
        ..
    } = vote_simulator;
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
    let ReplayBlockstoreComponents {
        blockstore,
        vote_simulator,
        ..
    } = replay_blockstore_components(
        Some(tr(0) / tr(1) / tr(2) / tr(3)),
        1,
        None::<GenerateVotes>,
    );
    let VoteSimulator {
        bank_forks,
        mut progress,
        ..
    } = vote_simulator;

    let bank0 = bank_forks.read().unwrap().get(0).unwrap();

    // Slot 4: 5 shreds, replay_fec_set_index=32; 5 < 32 so cleared.
    // Slot 8: 40 shreds, replay_fec_set_index=32; 40 > 32 so skipped.
    // Slot 12: 32 shreds, replay_fec_set_index=32, but no boundary replay
    // failure; a late signal must not clear this healthy bank.
    // Slot 16: 32 shreds and a boundary failure recorded for a different
    // replay offset; stale failure state must not authorize this restart.
    for (slot, shreds) in [(4, 5), (8, 40), (12, 32), (16, 32)] {
        let bank = Bank::new_from_parent(bank0.clone(), SlotLeader::default(), slot);
        bank_forks.write().unwrap().insert(bank);
        let mut p = ForkProgress::new(Hash::default(), Some(0), None, 0, 0, None);
        p.replay_progress.write().unwrap().num_shreds = shreds;
        if slot == 16 {
            p.mark_dead(DeadSlotReason::ReplayFailureAtUpdateParent(64));
        }
        progress.insert(slot, p);
    }

    let (tx, rx) = bounded(1024);
    for slot in [4, 8, 12, 16] {
        let parent_block_id = BlockId::new_unique();
        insert_update_parent_slot(
            &blockstore,
            slot,
            3, // optimistic block-header parent
            0, // UpdateParent parent (from ParentReady)
            parent_block_id,
            32,
        );
        tx.send(UpdateParentSignal { slot }).unwrap();
    }

    let cleared_bank_id = bank_forks.read().unwrap().get(4).unwrap().bank_id();
    let (replay_vote_sender, replay_vote_receiver) = bounded(1024);
    let (entry_notification_sender, entry_notification_receiver) = bounded(1);
    let (transaction_status_sender, purge_response_thread) = transaction_history_purge_responder();
    let mut async_verification_freelist = Vec::new();
    handle_update_parent_interrupts(
        &Pubkey::new_unique(),
        &blockstore,
        &bank_forks,
        &mut progress,
        &mut async_verification_freelist,
        &rx,
        &replay_vote_sender,
        &post_migration_status_for_tests(),
        Some(&entry_notification_sender),
        Some(&transaction_status_sender),
    )
    .unwrap();

    assert_eq!(
        purge_response_thread.join().unwrap(),
        (4, TransactionHistoryPurgeSource::UpdateParentSignal)
    );

    assert!(progress.get(&4).is_none()); // cleared: 5 < 32
    assert!(progress.get(&8).is_some()); // skipped: 40 > 32
    assert!(progress.get(&12).is_some()); // skipped: 32 == 32 without a replay failure
    assert!(progress.get(&16).is_some()); // skipped: boundary failure was recorded for 64
    assert_eq!(
        replay_vote_receiver.try_recv(),
        Ok(ReplayVoteMessage::InvalidBank {
            replay_bank_id: cleared_bank_id,
            replay_slot: 4,
        })
    );
    assert!(replay_vote_receiver.try_recv().is_err());
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
    let ReplayBlockstoreComponents {
        blockstore,
        vote_simulator,
        leader_schedule_cache,
        ..
    } = replay_blockstore_components(Some(tr(0)), 1, None::<GenerateVotes>);
    let VoteSimulator {
        bank_forks,
        mut progress,
        ..
    } = vote_simulator;
    let my_pubkey = Pubkey::new_unique();
    let slot = 4;
    let migration_status = post_migration_status_for_tests();

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
    let mut replay_timing = ReplayLoopTiming::default();
    let result = ReplayStage::generate_new_bank_forks(
        NewBankForksContext {
            blockstore: &blockstore,
            bank_forks: &bank_forks,
            leader_schedule_cache: &leader_schedule_cache,
            rpc_subscriptions: None,
            slot_status_notifier: &None,
            transaction_status_sender: Some(&transaction_status_sender),
            migration_status: &migration_status,
            my_pubkey: &my_pubkey,
        },
        &mut progress,
        &mut replay_timing,
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

    assert!(
        bank_forks.read().unwrap().get(slot).is_some(),
        "headerless UpdateParent should create a replay bank from the marker"
    );
    let replay_progress = progress.get(&slot).unwrap().replay_progress.read().unwrap();
    assert_eq!(replay_progress.num_shreds, 32);
    assert_eq!(replay_progress.num_txs, 0);
}

#[test]
fn test_update_parent_tower_gated() {
    let ReplayBlockstoreComponents {
        blockstore,
        vote_simulator,
        ..
    } = replay_blockstore_components(Some(tr(0) / tr(1)), 1, None::<GenerateVotes>);
    let VoteSimulator {
        bank_forks,
        mut progress,
        ..
    } = vote_simulator;

    let slot = 1;
    let parent_block_id = Hash::new_unique();
    let mut meta = blockstore.meta(slot).unwrap().unwrap();
    meta.parent_slot = Some(0);
    meta.parent_block_id = parent_block_id;
    meta.replay_fec_set_index = 10;
    blockstore.put_meta(slot, &meta).unwrap();

    let p = ForkProgress::new(Hash::default(), Some(0), None, 0, 0, None);
    p.replay_progress.write().unwrap().num_shreds = 5;
    progress.insert(slot, p);

    let (tx, rx) = bounded(1024);
    tx.send(UpdateParentSignal { slot }).unwrap();

    let (replay_vote_sender, _replay_vote_receiver) = bounded(1024);
    let mut async_verification_freelist = Vec::new();
    handle_update_parent_interrupts(
        &Pubkey::new_unique(),
        &blockstore,
        &bank_forks,
        &mut progress,
        &mut async_verification_freelist,
        &rx,
        &replay_vote_sender,
        &MigrationStatus::default(),
        None,
        None,
    )
    .unwrap();

    assert!(progress.get(&slot).is_some());
    assert!(bank_forks.read().unwrap().get(slot).is_some());
}

#[test]
fn test_update_parent_interrupt_ignores_non_first_leader_window_slot() {
    let ReplayBlockstoreComponents {
        blockstore,
        vote_simulator,
        ..
    } = replay_blockstore_components(Some(tr(0) / tr(1)), 1, None::<GenerateVotes>);
    let VoteSimulator {
        bank_forks,
        mut progress,
        ..
    } = vote_simulator;

    let slot = 1;
    let mut meta = blockstore.meta(slot).unwrap().unwrap();
    meta.parent_slot = Some(0);
    meta.parent_block_id = Hash::new_unique();
    meta.replay_fec_set_index = 10;
    blockstore.put_meta(slot, &meta).unwrap();

    let p = ForkProgress::new(Hash::default(), Some(0), None, 0, 0, None);
    p.replay_progress.write().unwrap().num_shreds = 5;
    progress.insert(slot, p);

    let (tx, rx) = bounded(1024);
    tx.send(UpdateParentSignal { slot }).unwrap();

    let (replay_vote_sender, _replay_vote_receiver) = bounded(1024);
    let mut async_verification_freelist = Vec::new();
    handle_update_parent_interrupts(
        &Pubkey::new_unique(),
        &blockstore,
        &bank_forks,
        &mut progress,
        &mut async_verification_freelist,
        &rx,
        &replay_vote_sender,
        &post_migration_status_for_tests(),
        None,
        None,
    )
    .unwrap();

    assert!(progress.get(&slot).is_some());
    assert!(bank_forks.read().unwrap().get(slot).is_some());
}

#[test]
fn test_update_parent_keeps_hard() {
    let ReplayBlockstoreComponents {
        blockstore,
        vote_simulator,
        ..
    } = replay_blockstore_components(Some(tr(0) / tr(1)), 1, None::<GenerateVotes>);
    let VoteSimulator {
        bank_forks,
        mut progress,
        ..
    } = vote_simulator;

    let slot = 1;
    let parent_block_id = Hash::new_unique();
    let mut meta = blockstore.meta(slot).unwrap().unwrap();
    meta.parent_slot = Some(0);
    meta.parent_block_id = parent_block_id;
    meta.replay_fec_set_index = 10;
    blockstore.put_meta(slot, &meta).unwrap();
    blockstore.set_dead_slot(slot).unwrap();

    let mut p = ForkProgress::new(Hash::default(), Some(0), None, 0, 0, None);
    p.replay_progress.write().unwrap().num_shreds = 5;
    p.mark_dead(DeadSlotReason::Hard);
    progress.insert(slot, p);

    let (tx, rx) = bounded(1024);
    tx.send(UpdateParentSignal { slot }).unwrap();

    let (replay_vote_sender, _replay_vote_receiver) = bounded(1024);
    let mut async_verification_freelist = Vec::new();
    handle_update_parent_interrupts(
        &Pubkey::new_unique(),
        &blockstore,
        &bank_forks,
        &mut progress,
        &mut async_verification_freelist,
        &rx,
        &replay_vote_sender,
        &post_migration_status_for_tests(),
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
    let ReplayBlockstoreComponents {
        blockstore,
        vote_simulator,
        ..
    } = replay_blockstore_components(Some(tr(0) / tr(1)), 1, None::<GenerateVotes>);
    let VoteSimulator {
        bank_forks,
        mut progress,
        ..
    } = vote_simulator;

    let slot = 1;
    let mut meta = blockstore.meta(slot).unwrap().unwrap();
    meta.replay_fec_set_index = 0;
    meta.last_index = None;
    blockstore.put_meta(slot, &meta).unwrap();

    let bank = bank_forks.read().unwrap().get(slot).unwrap();
    let p = ForkProgress::new(bank.last_blockhash(), Some(0), None, 0, 0, None);
    p.replay_progress.write().unwrap().num_shreds = 5;
    progress.insert(slot, p);

    let (replay_vote_sender, replay_vote_receiver) = bounded(1024);
    let (ancestor_hashes_replay_update_sender, _) = bounded(1024);
    let mut duplicate_slots_to_repair = DuplicateSlotsToRepair::default();
    let mut purge_repair_slot_counter = PurgeRepairSlotCounter::default();
    let migration_status = post_migration_status_for_tests();
    let mut dead_slot_context = dead_slot_context_for_tests(
        blockstore.clone(),
        replay_vote_sender.clone(),
        &ancestor_hashes_replay_update_sender,
        &mut duplicate_slots_to_repair,
        &mut purge_repair_slot_counter,
        None,
        &migration_status,
    );
    mark_replay_dead_slot(
        &bank,
        &BlockstoreProcessorError::InvalidTransaction(TransactionError::AccountNotFound),
        &mut progress,
        &mut dead_slot_context,
    );

    assert!(!blockstore.is_dead(slot));
    assert!(matches!(
        progress.get(&slot).unwrap().dead_reason,
        Some(DeadSlotReason::ReplayFailureBeforeUpdateParent)
    ));
    assert_eq!(
        replay_vote_receiver.try_recv(),
        Ok(ReplayVoteMessage::InvalidBank {
            replay_bank_id: bank.bank_id(),
            replay_slot: slot,
        })
    );
}

#[test]
fn test_after_update_hard_dead() {
    let ReplayBlockstoreComponents {
        blockstore,
        vote_simulator,
        ..
    } = replay_blockstore_components(Some(tr(0) / tr(1)), 1, None::<GenerateVotes>);
    let VoteSimulator {
        bank_forks,
        mut progress,
        ..
    } = vote_simulator;

    let slot = 1;
    let mut meta = blockstore.meta(slot).unwrap().unwrap();
    meta.replay_fec_set_index = 5;
    meta.last_index = None;
    blockstore.put_meta(slot, &meta).unwrap();

    let bank = bank_forks.read().unwrap().get(slot).unwrap();
    let p = ForkProgress::new(bank.last_blockhash(), Some(0), None, 0, 0, None);
    p.replay_progress.write().unwrap().num_shreds = 5;
    progress.insert(slot, p);

    let (replay_vote_sender, _replay_vote_receiver) = bounded(1024);
    let (ancestor_hashes_replay_update_sender, _) = bounded(1024);
    let mut duplicate_slots_to_repair = DuplicateSlotsToRepair::default();
    let mut purge_repair_slot_counter = PurgeRepairSlotCounter::default();
    let migration_status = post_migration_status_for_tests();
    let mut dead_slot_context = dead_slot_context_for_tests(
        blockstore.clone(),
        replay_vote_sender.clone(),
        &ancestor_hashes_replay_update_sender,
        &mut duplicate_slots_to_repair,
        &mut purge_repair_slot_counter,
        None,
        &migration_status,
    );
    mark_replay_dead_slot(
        &bank,
        &BlockstoreProcessorError::InvalidTransaction(TransactionError::AccountNotFound),
        &mut progress,
        &mut dead_slot_context,
    );

    assert!(blockstore.is_dead(slot));
    assert!(matches!(
        progress.get(&slot).unwrap().dead_reason,
        Some(DeadSlotReason::Hard)
    ));
}

#[test_case(31, false; "before_update_parent_restarts")]
#[test_case(32, false; "at_update_parent_restarts")]
#[test_case(33, true; "after_update_parent_is_hard_dead")]
fn test_spurious_update_parent_boundary(replayed_shreds: u64, should_be_hard: bool) {
    let ReplayBlockstoreComponents {
        blockstore,
        vote_simulator,
        ..
    } = replay_blockstore_components(Some(tr(0)), 1, None::<GenerateVotes>);
    let VoteSimulator {
        bank_forks,
        mut progress,
        ..
    } = vote_simulator;

    let slot = 4;
    let bank0 = bank_forks.read().unwrap().get(0).unwrap();
    let bank = Bank::new_from_parent(bank0, SlotLeader::default(), slot);
    let bank = bank_forks.write().unwrap().insert(bank);
    let header = VersionedBlockMarker::from_block_header(BlockHeaderV1 {
        parent_slot: 0,
        parent_block_id: Hash::default(),
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
    let p = ForkProgress::new(bank.last_blockhash(), Some(0), None, 0, 0, None);
    p.replay_progress.write().unwrap().num_shreds = replayed_shreds;
    progress.insert(slot, p);

    let (replay_vote_sender, _replay_vote_receiver) = bounded(1024);
    let (ancestor_hashes_replay_update_sender, _) = bounded(1024);
    let mut duplicate_slots_to_repair = DuplicateSlotsToRepair::default();
    let mut purge_repair_slot_counter = PurgeRepairSlotCounter::default();
    let migration_status = post_migration_status_for_tests();
    let mut dead_slot_context = dead_slot_context_for_tests(
        blockstore.clone(),
        replay_vote_sender.clone(),
        &ancestor_hashes_replay_update_sender,
        &mut duplicate_slots_to_repair,
        &mut purge_repair_slot_counter,
        None,
        &migration_status,
    );
    mark_replay_dead_slot(
        &bank,
        &BlockstoreProcessorError::BlockComponentProcessor(
            BlockComponentProcessorError::SpuriousUpdateParent,
        ),
        &mut progress,
        &mut dead_slot_context,
    );
    drop(dead_slot_context);

    if should_be_hard {
        assert!(blockstore.is_dead(slot));
        assert!(matches!(
            progress.get(&slot).unwrap().dead_reason,
            Some(DeadSlotReason::Hard)
        ));
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

    let mut async_verification_freelist = Vec::new();
    process_soft_dead_slots(
        &Pubkey::new_unique(),
        &blockstore,
        &bank_forks,
        &None,
        &None,
        &mut progress,
        &mut async_verification_freelist,
        &replay_vote_sender,
        &migration_status,
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
    let ReplayBlockstoreComponents {
        blockstore,
        vote_simulator,
        ..
    } = replay_blockstore_components(Some(tr(0)), 1, None::<GenerateVotes>);
    let VoteSimulator {
        bank_forks,
        mut progress,
        ..
    } = vote_simulator;

    let slot = 4;
    let bank0 = bank_forks.read().unwrap().get(0).unwrap();
    let bank = Bank::new_from_parent(bank0, SlotLeader::default(), slot);
    let bank = bank_forks.write().unwrap().insert(bank);
    let header = VersionedBlockMarker::from_block_header(BlockHeaderV1 {
        parent_slot: 0,
        parent_block_id: Hash::default(),
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

    let p = ForkProgress::new(bank.last_blockhash(), Some(0), None, 0, 0, None);
    p.replay_progress.write().unwrap().num_shreds = 5;
    progress.insert(slot, p);

    let (replay_vote_sender, _replay_vote_receiver) = bounded(1024);
    let (ancestor_hashes_replay_update_sender, _) = bounded(1024);
    let mut duplicate_slots_to_repair = DuplicateSlotsToRepair::default();
    let mut purge_repair_slot_counter = PurgeRepairSlotCounter::default();
    let migration_status = post_migration_status_for_tests();
    let mut dead_slot_context = dead_slot_context_for_tests(
        blockstore.clone(),
        replay_vote_sender.clone(),
        &ancestor_hashes_replay_update_sender,
        &mut duplicate_slots_to_repair,
        &mut purge_repair_slot_counter,
        None,
        &migration_status,
    );
    mark_replay_dead_slot(
        &bank,
        &BlockstoreProcessorError::BlockComponentProcessor(
            BlockComponentProcessorError::MissingParentMarker,
        ),
        &mut progress,
        &mut dead_slot_context,
    );

    assert!(!blockstore.is_dead(slot));
    assert!(matches!(
        progress.get(&slot).unwrap().dead_reason,
        Some(DeadSlotReason::ReplayFailureBeforeUpdateParent)
    ));
}

#[test]
fn test_soft_dead_restarts() {
    let ReplayBlockstoreComponents {
        blockstore,
        vote_simulator,
        ..
    } = replay_blockstore_components(Some(tr(0) / tr(1)), 1, None::<GenerateVotes>);
    let VoteSimulator {
        bank_forks,
        mut progress,
        ..
    } = vote_simulator;

    let slot = 4;
    let parent_block_id = BlockId::new_unique();
    insert_update_parent_slot(&blockstore, slot, 3, 0, parent_block_id, 32);
    let bank0 = bank_forks.read().unwrap().get(0).unwrap();
    let bank = Bank::new_from_parent(bank0, SlotLeader::default(), slot);
    bank_forks.write().unwrap().insert(bank);

    let mut p = ForkProgress::new(Hash::default(), Some(0), None, 0, 0, None);
    p.replay_progress.write().unwrap().num_shreds = 5;
    p.mark_dead(DeadSlotReason::ReplayFailureBeforeUpdateParent);
    progress.insert(slot, p);

    let (replay_vote_sender, _replay_vote_receiver) = bounded(1024);
    let (transaction_status_sender, purge_response_thread) = transaction_history_purge_responder();
    let mut async_verification_freelist = Vec::new();
    process_soft_dead_slots(
        &Pubkey::new_unique(),
        &blockstore,
        &bank_forks,
        &None,
        &None,
        &mut progress,
        &mut async_verification_freelist,
        &replay_vote_sender,
        &post_migration_status_for_tests(),
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
    let ReplayBlockstoreComponents {
        blockstore,
        vote_simulator,
        ..
    } = replay_blockstore_components(Some(tr(0) / tr(1)), 1, None::<GenerateVotes>);
    let VoteSimulator {
        bank_forks,
        mut progress,
        ..
    } = vote_simulator;

    let slot = 1;
    let mut meta = blockstore.meta(slot).unwrap().unwrap();
    meta.replay_fec_set_index = 0;
    meta.consumed = 1;
    meta.last_index = Some(0);
    blockstore.put_meta(slot, &meta).unwrap();

    let mut p = ForkProgress::new(Hash::default(), Some(0), None, 0, 0, None);
    p.mark_dead(DeadSlotReason::ReplayFailureBeforeUpdateParent);
    progress.insert(slot, p);

    let (replay_vote_sender, _replay_vote_receiver) = bounded(1024);
    let mut async_verification_freelist = Vec::new();
    process_soft_dead_slots(
        &Pubkey::new_unique(),
        &blockstore,
        &bank_forks,
        &None,
        &None,
        &mut progress,
        &mut async_verification_freelist,
        &replay_vote_sender,
        &post_migration_status_for_tests(),
        None,
        None,
    )
    .unwrap();

    assert!(blockstore.is_dead(slot));
    assert!(matches!(
        progress.get(&slot).unwrap().dead_reason,
        Some(DeadSlotReason::Hard)
    ));
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

    let latest = receiver.try_recv().unwrap();
    assert_eq!(latest.start_slot, 12);
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

    let latest = receiver.try_recv().unwrap();
    assert_eq!(latest.end_slot, 23);
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

    let latest = receiver.try_recv().unwrap();
    assert_eq!(latest.start_slot, 20);
    assert!(receiver.try_recv().is_err());
}

#[test]
fn test_replay_own_update_full() {
    let (vote_simulator, blockstore) = setup_forks_from_tree(tr(0), 1, None::<GenerateVotes>);
    let VoteSimulator {
        bank_forks,
        mut progress,
        validator_keypairs,
        ..
    } = vote_simulator;
    let my_pubkey = *validator_keypairs.keys().next().unwrap();
    let root_bank = bank_forks.read().unwrap().root_bank();
    let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&root_bank));
    let slot = 4;
    let replay_fec_set_index = 32;

    insert_update_parent_slot(
        &blockstore,
        slot,
        1,
        0,
        BlockId::new_unique(),
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

    let mut replay_timing = ReplayLoopTiming::default();
    let migration_status = post_migration_status_for_tests();
    ReplayStage::generate_new_bank_forks(
        NewBankForksContext {
            blockstore: &blockstore,
            bank_forks: &bank_forks,
            leader_schedule_cache: &leader_schedule_cache,
            rpc_subscriptions: None,
            slot_status_notifier: &None,
            transaction_status_sender: None,
            migration_status: &migration_status,
            my_pubkey: &my_pubkey,
        },
        &mut progress,
        &mut replay_timing,
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
fn test_purge_anc_desc() {
    let (VoteSimulator { bank_forks, .. }, _) = setup_default_forks(1, None::<GenerateVotes>);

    // Purge branch rooted at slot 2
    let mut descendants = bank_forks.read().unwrap().descendants();
    let mut ancestors = bank_forks.read().unwrap().ancestors();
    let slot_2_descendants = descendants.get(&2).unwrap().clone();
    ReplayStage::purge_ancestors_descendants(
        2,
        &slot_2_descendants,
        &mut ancestors,
        &mut descendants,
    );

    // Result should be equivalent to removing slot from BankForks
    // and regenerating the `ancestor` `descendant` maps
    for d in slot_2_descendants {
        bank_forks.write().unwrap().remove(d);
    }
    bank_forks.write().unwrap().remove(2);
    assert!(check_map_eq(
        &ancestors,
        &bank_forks.read().unwrap().ancestors()
    ));
    assert!(check_map_eq(
        &descendants,
        &bank_forks.read().unwrap().descendants()
    ));

    // Try to purge the root
    bank_forks.write().unwrap().set_root(3, None, None);
    let mut descendants = bank_forks.read().unwrap().descendants();
    let mut ancestors = bank_forks.read().unwrap().ancestors();
    let slot_3_descendants = descendants.get(&3).unwrap().clone();
    ReplayStage::purge_ancestors_descendants(
        3,
        &slot_3_descendants,
        &mut ancestors,
        &mut descendants,
    );

    assert!(ancestors.is_empty());
    // Only remaining keys should be ones < root
    for k in descendants.keys() {
        assert!(*k < 3);
    }
}

#[test]
fn test_leader_snapshot_restart_propagation() {
    let ReplayBlockstoreComponents {
        validator_node_to_vote_keys,
        leader_schedule_cache,
        vote_simulator,
        ..
    } = replay_blockstore_components(None, 1, None::<GenerateVotes>);

    let VoteSimulator {
        mut progress,
        bank_forks,
        ..
    } = vote_simulator;
    let mut vote_slots = HashSet::default();

    let root_bank = bank_forks.read().unwrap().root_bank();
    let my_pubkey = leader_schedule_cache
        .slot_leader_at(root_bank.slot(), Some(&root_bank))
        .unwrap()
        .id;

    // Check that we are the leader of the root bank
    assert!(
        progress
            .get_propagated_stats(root_bank.slot())
            .unwrap()
            .is_leader_slot
    );
    let ancestors = bank_forks.read().unwrap().ancestors();

    // Freeze bank so it shows up in frozen banks
    root_bank.freeze();
    let mut frozen_banks: Vec<_> = bank_forks
        .read()
        .unwrap()
        .frozen_banks()
        .map(|(_slot, bank)| bank)
        .collect();

    // Compute bank stats, make sure vote is propagated back to starting root bank
    let vote_tracker = VoteTracker::default();

    // Add votes
    for vote_key in validator_node_to_vote_keys.values() {
        vote_tracker.insert_vote(root_bank.slot(), *vote_key);
    }

    assert!(
        !progress
            .get_leader_propagation_slot_must_exist(root_bank.slot())
            .0
    );

    // Update propagation status
    let mut tower = Tower::new_for_tests(0, 0.67);
    ReplayStage::compute_bank_stats(
        &validator_node_to_vote_keys[&my_pubkey],
        &ancestors,
        &mut frozen_banks,
        &mut tower,
        &mut progress,
        &vote_tracker,
        &ClusterSlots::default_for_tests(),
        &bank_forks,
        &mut HeaviestSubtreeForkChoice::new_from_bank_forks(bank_forks.clone()),
        &mut LatestValidatorVotesForFrozenBanks::default(),
        &mut vote_slots,
        &MigrationStatus::default(),
    );

    // Check status is true
    assert!(
        progress
            .get_leader_propagation_slot_must_exist(root_bank.slot())
            .0
    );
}

#[test]
fn test_dump_then_repair_correct_slots() {
    // Create the tree of banks in a BankForks object
    let forks = tr(0) / (tr(1)) / (tr(2));

    let ReplayBlockstoreComponents {
        ref mut vote_simulator,
        ref blockstore,
        ref leader_schedule_cache,
        ..
    } = replay_blockstore_components(Some(forks), 1, None);

    let &mut VoteSimulator {
        ref mut progress,
        ref bank_forks,
        ..
    } = vote_simulator;

    let (mut ancestors, mut descendants) = {
        let r_bank_forks = bank_forks.read().unwrap();
        (r_bank_forks.ancestors(), r_bank_forks.descendants())
    };

    // Insert different versions of both 1 and 2. Both slots 1 and 2 should
    // then be purged
    let mut duplicate_slots_to_repair = DuplicateSlotsToRepair::default();
    duplicate_slots_to_repair.insert(1, Hash::new_unique());
    duplicate_slots_to_repair.insert(2, Hash::new_unique());
    let mut purge_repair_slot_counter = PurgeRepairSlotCounter::default();
    let (dumped_slots_sender, dumped_slots_receiver) = bounded(1024);
    let should_be_dumped = duplicate_slots_to_repair
        .iter()
        .map(|(&s, &h)| (s, h))
        .collect_vec();

    ReplayStage::dump_then_repair_correct_slots(
        &mut duplicate_slots_to_repair,
        &mut ancestors,
        &mut descendants,
        progress,
        bank_forks,
        blockstore,
        None,
        &mut purge_repair_slot_counter,
        &dumped_slots_sender,
        &Pubkey::new_unique(),
        leader_schedule_cache,
    );
    assert_eq!(should_be_dumped, dumped_slots_receiver.recv().ok().unwrap());

    let r_bank_forks = bank_forks.read().unwrap();
    for slot in 0..=2 {
        let bank = r_bank_forks.get(slot);
        let ancestor_result = ancestors.get(&slot);
        let descendants_result = descendants.get(&slot);
        if slot == 0 {
            assert!(bank.is_some());
            assert!(ancestor_result.is_some());
            assert!(descendants_result.is_some());
        } else {
            assert!(bank.is_none());
            assert!(ancestor_result.is_none());
            assert!(descendants_result.is_none());
        }
    }
    assert_eq!(2, purge_repair_slot_counter.len());
    assert_eq!(1, *purge_repair_slot_counter.get(&1).unwrap());
    assert_eq!(1, *purge_repair_slot_counter.get(&2).unwrap());
}

fn receive_slots(retransmit_slots_receiver: &Receiver<Slot>) -> Vec<Slot> {
    let mut slots = Vec::default();
    while let Ok(slot) = retransmit_slots_receiver.recv_timeout(Duration::from_millis(10)) {
        slots.push(slot);
    }
    slots
}

#[test]
fn test_maybe_retransmit_unpropagated_slots() {
    let ReplayBlockstoreComponents {
        validator_node_to_vote_keys,
        leader_schedule_cache,
        vote_simulator,
        ..
    } = replay_blockstore_components(None, 10, None::<GenerateVotes>);

    let VoteSimulator {
        mut progress,
        ref bank_forks,
        ..
    } = vote_simulator;

    let (retransmit_slots_sender, retransmit_slots_receiver) = bounded(1024);
    let retry_time = Instant::now()
        .checked_sub(Duration::from_millis(RETRANSMIT_BASE_DELAY_MS + 1))
        .unwrap();
    progress.get_retransmit_info_mut(0).unwrap().retry_time = retry_time;

    let mut prev_index = 0;
    for i in (1..10).chain(11..15) {
        let bank = Bank::new_from_parent(
            bank_forks.read().unwrap().get(prev_index).unwrap(),
            leader_schedule_cache.slot_leader_at(i, None).unwrap(),
            i,
        );
        progress.insert(
            i,
            ForkProgress::new_from_bank(
                &bank,
                bank.leader_id(),
                validator_node_to_vote_keys.get(bank.leader_id()).unwrap(),
                Some(0),
                0,
                0,
                None,
            ),
        );
        assert!(progress.get_propagated_stats(i).unwrap().is_leader_slot);
        bank.freeze();
        bank_forks.write().unwrap().insert(bank);
        prev_index = i;
        progress.get_retransmit_info_mut(i).unwrap().retry_time = retry_time;
    }

    // expect single slot when latest_leader_slot is the start of a consecutive range
    let latest_leader_slot = 0;
    ReplayStage::maybe_retransmit_unpropagated_slots(
        "test",
        &retransmit_slots_sender,
        &mut progress,
        latest_leader_slot,
    );
    let received_slots = receive_slots(&retransmit_slots_receiver);
    assert_eq!(received_slots, vec![0]);

    // expect range of slots from start of consecutive slots
    let latest_leader_slot = 6;
    ReplayStage::maybe_retransmit_unpropagated_slots(
        "test",
        &retransmit_slots_sender,
        &mut progress,
        latest_leader_slot,
    );
    let received_slots = receive_slots(&retransmit_slots_receiver);
    assert_eq!(received_slots, vec![4, 5, 6]);

    // expect range of slots skipping a discontinuity in the range
    let latest_leader_slot = 11;
    ReplayStage::maybe_retransmit_unpropagated_slots(
        "test",
        &retransmit_slots_sender,
        &mut progress,
        latest_leader_slot,
    );
    let received_slots = receive_slots(&retransmit_slots_receiver);
    assert_eq!(received_slots, vec![8, 9, 11]);
}

#[test]
fn test_dumped_slot_not_causing_panic() {
    agave_logger::setup();
    let ReplayBlockstoreComponents {
        validator_node_to_vote_keys,
        leader_schedule_cache,
        poh_recorder,
        mut poh_controller,
        vote_simulator,
        rpc_subscriptions,
        ref my_pubkey,
        ref blockstore,
        ..
    } = replay_blockstore_components(None, 10, None::<GenerateVotes>);

    let VoteSimulator {
        mut progress,
        ref bank_forks,
        ..
    } = vote_simulator;

    let poh_recorder = Arc::new(poh_recorder);
    let (retransmit_slots_sender, _) = bounded(1024);

    // Use a bank slot when I was not leader to avoid panic for dumping my own slot
    let slot_to_dump = (1..100)
        .find(|i| {
            leader_schedule_cache
                .slot_leader_at(*i, None)
                .map(|leader| leader.id)
                != Some(*my_pubkey)
        })
        .unwrap();
    let bank_to_dump = Bank::new_from_parent(
        bank_forks.read().unwrap().get(0).unwrap(),
        leader_schedule_cache
            .slot_leader_at(slot_to_dump, None)
            .unwrap(),
        slot_to_dump,
    );
    progress.insert(
        slot_to_dump,
        ForkProgress::new_from_bank(
            &bank_to_dump,
            bank_to_dump.leader_id(),
            validator_node_to_vote_keys
                .get(bank_to_dump.leader_id())
                .unwrap(),
            Some(0),
            0,
            0,
            None,
        ),
    );
    assert!(progress.get_propagated_stats(slot_to_dump).is_some());
    bank_to_dump.freeze();
    bank_forks.write().unwrap().insert(bank_to_dump);
    let bank_to_dump = bank_forks
        .read()
        .unwrap()
        .get(slot_to_dump)
        .expect("Just inserted");

    progress.get_retransmit_info_mut(0).unwrap().retry_time = Instant::now();
    poh_controller
        .reset_sync(bank_to_dump, Some((slot_to_dump + 1, slot_to_dump + 1)))
        .unwrap();
    assert_eq!(poh_recorder.read().unwrap().start_slot(), slot_to_dump);

    // Now dump and repair slot_to_dump
    let (mut ancestors, mut descendants) = {
        let r_bank_forks = bank_forks.read().unwrap();
        (r_bank_forks.ancestors(), r_bank_forks.descendants())
    };
    let mut duplicate_slots_to_repair = DuplicateSlotsToRepair::default();
    let bank_to_dump_bad_hash = Hash::new_unique();
    duplicate_slots_to_repair.insert(slot_to_dump, bank_to_dump_bad_hash);
    let mut purge_repair_slot_counter = PurgeRepairSlotCounter::default();
    let (dumped_slots_sender, dumped_slots_receiver) = bounded(1024);

    ReplayStage::dump_then_repair_correct_slots(
        &mut duplicate_slots_to_repair,
        &mut ancestors,
        &mut descendants,
        &mut progress,
        bank_forks,
        blockstore,
        None,
        &mut purge_repair_slot_counter,
        &dumped_slots_sender,
        my_pubkey,
        &leader_schedule_cache,
    );
    assert_eq!(
        dumped_slots_receiver.recv_timeout(Duration::from_secs(1)),
        Ok(vec![(slot_to_dump, bank_to_dump_bad_hash)])
    );

    // Now check it doesn't cause panic in the following functions.
    ReplayStage::retransmit_latest_unpropagated_leader_slot(
        &poh_recorder,
        &retransmit_slots_sender,
        &mut progress,
    );

    let (banking_tracer, _) = BankingTracer::new(None).unwrap();
    // A vote has not technically been rooted, but it doesn't matter for
    // this test to use true to avoid skipping the leader slot
    let has_new_vote_been_rooted = true;

    let rpc_subscriptions = Some(rpc_subscriptions);

    assert!(
        ReplayStage::maybe_start_leader(
            my_pubkey,
            bank_forks,
            &poh_recorder,
            &mut poh_controller,
            &leader_schedule_cache,
            rpc_subscriptions.as_deref(),
            &None,
            &mut progress,
            &retransmit_slots_sender,
            &mut SkippedSlotsInfo::default(),
            &banking_tracer,
            has_new_vote_been_rooted,
            &MigrationStatus::default(),
        )
        .is_none()
    );
}

#[test]
#[should_panic(expected = "We are attempting to dump a block that we produced")]
fn test_dump_own_slots_fails() {
    // Create the tree of banks in a BankForks object
    let forks = tr(0) / (tr(1)) / (tr(2));

    let ReplayBlockstoreComponents {
        ref mut vote_simulator,
        ref blockstore,
        ref my_pubkey,
        ref leader_schedule_cache,
        ..
    } = replay_blockstore_components(Some(forks), 1, None);

    let &mut VoteSimulator {
        ref mut progress,
        ref bank_forks,
        ..
    } = vote_simulator;

    let (mut ancestors, mut descendants) = {
        let r_bank_forks = bank_forks.read().unwrap();
        (r_bank_forks.ancestors(), r_bank_forks.descendants())
    };

    // Insert different versions of both 1 and 2. Although normally these slots would be dumped,
    // because we were the leader for these slots we should panic
    let mut duplicate_slots_to_repair = DuplicateSlotsToRepair::default();
    duplicate_slots_to_repair.insert(1, Hash::new_unique());
    duplicate_slots_to_repair.insert(2, Hash::new_unique());
    let mut purge_repair_slot_counter = PurgeRepairSlotCounter::default();
    let (dumped_slots_sender, _) = bounded(1024);

    ReplayStage::dump_then_repair_correct_slots(
        &mut duplicate_slots_to_repair,
        &mut ancestors,
        &mut descendants,
        progress,
        bank_forks,
        blockstore,
        None,
        &mut purge_repair_slot_counter,
        &dumped_slots_sender,
        my_pubkey,
        leader_schedule_cache,
    );
}

type GenerateVotes = Box<dyn Fn(Vec<Pubkey>) -> HashMap<Pubkey, Vec<Slot>>>;

pub fn setup_forks_from_tree(
    tree: Tree<Slot>,
    num_keys: usize,
    generate_votes: Option<GenerateVotes>,
) -> (VoteSimulator, Blockstore) {
    let ledger_path = get_tmp_ledger_path!();
    let mut vote_simulator = VoteSimulator::new_with(
        num_keys,
        BankTestConfig {
            accounts_db_config: AccountsDbConfig {
                bank_hash_details_dir: ledger_path.clone(),
                ..ACCOUNTS_DB_CONFIG_FOR_TESTING
            },
        },
    );
    let pubkeys: Vec<Pubkey> = vote_simulator
        .validator_keypairs
        .values()
        .map(|k| k.node_keypair.pubkey())
        .collect();
    let cluster_votes = generate_votes
        .map(|generate_votes| generate_votes(pubkeys))
        .unwrap_or_default();
    vote_simulator.fill_bank_forks(tree.clone(), &cluster_votes, true);
    let blockstore = Blockstore::open(&ledger_path).unwrap();
    blockstore.add_tree(tree, false, true, 2, Hash::default());
    (vote_simulator, blockstore)
}

fn setup_default_forks(
    num_keys: usize,
    generate_votes: Option<GenerateVotes>,
) -> (VoteSimulator, Blockstore) {
    /*
        Build fork structure:

             slot 0
               |
             slot 1
             /    \
        slot 2    |
           |    slot 3
        slot 4    |
                slot 5
                  |
                slot 6
    */

    let tree = tr(0) / (tr(1) / (tr(2) / (tr(4))) / (tr(3) / (tr(5) / (tr(6)))));
    setup_forks_from_tree(tree, num_keys, generate_votes)
}

fn check_map_eq<K: Eq + std::hash::Hash + std::fmt::Debug, T: PartialEq + std::fmt::Debug>(
    map1: &HashMap<K, T>,
    map2: &HashMap<K, T>,
) -> bool {
    map1.len() == map2.len() && map1.iter().all(|(k, v)| map2.get(k).unwrap() == v)
}

// test-helper to wait for poh service to pick up and handle poh controller messages
fn wait_for_poh_service(poh_controller: &PohController) {
    while poh_controller.has_pending_message() {
        std::hint::spin_loop();
    }
}

#[test]
fn test_skip_leader_slot_for_existing_slot() {
    agave_logger::setup();

    let ReplayBlockstoreComponents {
        blockstore,
        my_pubkey,
        leader_schedule_cache,
        poh_recorder,
        mut poh_controller,
        vote_simulator,
        rpc_subscriptions,
        ..
    } = replay_blockstore_components(None, 1, None);

    let VoteSimulator {
        bank_forks,
        mut progress,
        ..
    } = vote_simulator;

    let working_bank = bank_forks.read().unwrap().working_bank();
    assert!(working_bank.is_complete());
    assert!(working_bank.is_frozen());

    // Insert a block two slots greater than current bank. This slot does
    // not have a corresponding Bank in BankForks; this emulates a scenario
    // where the block had previously been created and added to BankForks,
    // but then got removed. This could be the case if the Bank was not on
    // the major fork.
    let dummy_slot = working_bank.slot() + 2;
    let initial_slot = working_bank.slot();
    let num_entries = 10;
    let (shreds, _) = make_slot_entries(dummy_slot, initial_slot, num_entries);
    blockstore.insert_shreds(shreds, false).unwrap();

    // Reset PoH recorder to the completed bank to ensure consistent state
    ReplayStage::reset_poh_recorder(
        &my_pubkey,
        &blockstore,
        working_bank.clone(),
        &mut poh_controller,
        &leader_schedule_cache,
    );
    wait_for_poh_service(&poh_controller);

    // Register just over one slot worth of ticks directly with PoH recorder
    let num_poh_ticks =
        (working_bank.ticks_per_slot() * working_bank.hashes_per_tick().unwrap()) + 1;
    poh_recorder
        .write()
        .map(|mut poh_recorder| {
            for _ in 0..num_poh_ticks + 1 {
                poh_recorder.tick();
            }
        })
        .unwrap();

    let poh_recorder = Arc::new(poh_recorder);
    let (retransmit_slots_sender, _) = bounded(1024);
    let (banking_tracer, _) = BankingTracer::new(None).unwrap();
    // A vote has not technically been rooted, but it doesn't matter for
    // this test to use true to avoid skipping the leader slot
    let has_new_vote_been_rooted = true;

    let rpc_subscriptions = Some(rpc_subscriptions);

    // We should not attempt to start leader for the dummy_slot
    assert_matches!(
        poh_recorder.read().unwrap().reached_leader_slot(&my_pubkey),
        PohLeaderStatus::NotReached
    );
    assert!(
        ReplayStage::maybe_start_leader(
            &my_pubkey,
            &bank_forks,
            &poh_recorder,
            &mut poh_controller,
            &leader_schedule_cache,
            rpc_subscriptions.as_deref(),
            &None,
            &mut progress,
            &retransmit_slots_sender,
            &mut SkippedSlotsInfo::default(),
            &banking_tracer,
            has_new_vote_been_rooted,
            &MigrationStatus::default(),
        )
        .is_none()
    );

    // Register another slots worth of ticks  with PoH recorder
    poh_recorder
        .write()
        .map(|mut poh_recorder| {
            for _ in 0..num_poh_ticks + 1 {
                poh_recorder.tick();
            }
        })
        .unwrap();

    // We should now start leader for dummy_slot + 1
    let good_slot = dummy_slot + 1;
    assert!(
        ReplayStage::maybe_start_leader(
            &my_pubkey,
            &bank_forks,
            &poh_recorder,
            &mut poh_controller,
            &leader_schedule_cache,
            rpc_subscriptions.as_deref(),
            &None,
            &mut progress,
            &retransmit_slots_sender,
            &mut SkippedSlotsInfo::default(),
            &banking_tracer,
            has_new_vote_been_rooted,
            &MigrationStatus::default(),
        )
        .is_some()
    );
    wait_for_poh_service(&poh_controller);

    // Get the new working bank, which is also the new leader bank/slot
    let working_bank = bank_forks.read().unwrap().working_bank();
    // The new bank's slot must NOT be dummy_slot as the blockstore already
    // had a shred inserted for dummy_slot prior to maybe_start_leader().
    // maybe_start_leader() must not pick dummy_slot to avoid creating a
    // duplicate block.
    assert_eq!(working_bank.slot(), good_slot);
    assert_eq!(working_bank.parent_slot(), initial_slot);
}
