use {
    super::*,
    crate::{
        commitment_service::AggregateCommitmentService,
        consensus::{
            ThresholdDecision, Tower, VOTE_THRESHOLD_DEPTH,
            progress_map::{DeadSlotReason, RETRANSMIT_BASE_DELAY_MS, ValidatorStakeInfo},
            tower_storage::{FileTowerStorage, NullTowerStorage},
            tree_diff::TreeDiff,
        },
        replay_stage::ReplayStage,
        vote_simulator::{self, VoteSimulator},
    },
    agave_votor_messages::{
        certificate::{CertSignature, GenesisCert},
        consensus_message::Block,
    },
    blockstore_processor::{
        AsyncVerificationProgress, ConfirmationProgress, ProcessOptions, confirm_full_slot,
        fill_blockstore_slot_with_ticks, process_bank_0,
    },
    crossbeam_channel::bounded,
    itertools::Itertools,
    solana_account::{ReadableAccount, state_traits::StateMut},
    solana_accounts_db::accounts_db::{ACCOUNTS_DB_CONFIG_FOR_TESTING, AccountsDbConfig},
    solana_bls_signatures::{BLS_SIGNATURE_AFFINE_SIZE, Signature as BLSSignature},
    solana_client::connection_cache::ConnectionCache,
    solana_entry::{
        block_component::{
            BlockComponent, BlockFooterV1, BlockHeaderV1, UpdateParentV1, VersionedBlockMarker,
            VersionedUpdateParent,
        },
        entry::{self, Entry},
    },
    solana_genesis_config as genesis_config,
    solana_gossip::{crds::Cursor, node::Node},
    solana_hash::Hash,
    solana_instruction::error::InstructionError,
    solana_keypair::Keypair,
    solana_leader_schedule::NUM_CONSECUTIVE_LEADER_SLOTS as NUM_CONSECUTIVE_LEADER_SLOTS_NZ,
    solana_ledger::{
        block_error::BlockError,
        blockstore::{
            BlockstoreError, UpdateParentSignal, entries_to_test_shreds, make_slot_entries,
        },
        create_new_tmp_ledger,
        genesis_utils::{create_genesis_config, create_genesis_config_with_leader},
        get_tmp_ledger_path, get_tmp_ledger_path_auto_delete,
        shred::{ProcessShredsStats, ReedSolomonCache, Shred, Shredder},
    },
    solana_net_utils::SocketAddrSpace,
    solana_poh::poh_recorder::create_test_recorder,
    solana_poh_config::PohConfig,
    solana_rpc::{
        optimistically_confirmed_bank_tracker::OptimisticallyConfirmedBank,
        rpc::{create_test_transaction_entries, populate_blockstore_for_tests},
        slot_status_notifier::SlotStatusNotifierInterface,
    },
    solana_runtime::{
        bank::BankTestConfig,
        block_component_processor::BlockComponentProcessorError,
        commitment::{BlockCommitment, VOTE_THRESHOLD_SIZE},
        genesis_utils::{GenesisConfigInfo, ValidatorVoteKeypairs},
    },
    solana_sha256_hasher::hash,
    solana_shred_version::compute_shred_version,
    solana_signature::Signature,
    solana_svm_timings::ExecuteTimings,
    solana_system_transaction as system_transaction,
    solana_tpu_client::tpu_client::{DEFAULT_TPU_CONNECTION_POOL_SIZE, DEFAULT_VOTE_USE_QUIC},
    solana_transaction_error::TransactionError,
    solana_transaction_status::VersionedTransactionWithStatusMeta,
    solana_unified_scheduler_pool::DefaultSchedulerPool,
    solana_vote::vote_transaction,
    solana_vote_program::vote_state::{
        self, TowerSync, VoteStateV4, VoteStateVersions, handler::VoteStateHandler,
    },
    std::{
        fs::remove_dir_all,
        iter,
        sync::{Arc, Barrier, Mutex, RwLock, atomic::AtomicU64},
    },
    tempfile::tempdir,
    test_case::test_case,
    trees::{Tree, tr},
};

const NUM_CONSECUTIVE_LEADER_SLOTS: Slot = NUM_CONSECUTIVE_LEADER_SLOTS_NZ.get() as Slot;

static_assertions::const_assert!(REFRESH_VOTE_BLOCKHEIGHT < solana_clock::MAX_PROCESSING_AGE);

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
        let replay_tx_thread_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .thread_name(|i| format!("solReplayTest{i:02}"))
            .build()
            .expect("new rayon threadpool");
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
            replay_tx_thread_pool,
            migration_status,
        }
    }
}

fn post_migration_status_for_tests() -> MigrationStatus {
    let migration_status = MigrationStatus::default();
    migration_status.record_feature_activation(0);
    let genesis_block = Block {
        slot: 0,
        block_id: Hash::default(),
    };
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
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        )
        .collect()
}

fn insert_update_parent_slot(
    blockstore: &Blockstore,
    slot: Slot,
    block_header_parent_slot: Slot,
    update_parent_slot: Slot,
    update_parent_block_id: Hash,
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
    cluster_info: ClusterInfo,
    pub(crate) leader_schedule_cache: Arc<LeaderScheduleCache>,
    poh_recorder: Arc<RwLock<PohRecorder>>,
    poh_controller: PohController,
    tower: Tower,
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

    // Tower
    let my_vote_pubkey = my_keypairs.vote_keypair.pubkey();
    let tower = Tower::new_from_bankforks(
        &bank_forks.read().unwrap(),
        &cluster_info.id(),
        &my_vote_pubkey,
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
        cluster_info,
        leader_schedule_cache,
        poh_recorder,
        poh_controller,
        tower,
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
            migration_status: &MigrationStatus::default(),
            my_pubkey: &Pubkey::default(),
        },
        &mut progress,
        &mut replay_timing,
    );
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
            migration_status: &MigrationStatus::default(),
            my_pubkey: &Pubkey::default(),
        },
        &mut progress,
        &mut replay_timing,
    );
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

#[test]
fn test_dead_fork_transaction_error() {
    let keypair1 = Keypair::new();
    let keypair2 = Keypair::new();
    let missing_keypair = Keypair::new();
    let missing_keypair2 = Keypair::new();

    let res = check_dead_fork(|_keypair, bank| {
        let blockhash = bank.last_blockhash();
        let slot = bank.slot();
        let hashes_per_tick = bank.hashes_per_tick().unwrap_or(0);
        let entry = entry::next_entry(
            &blockhash,
            hashes_per_tick.saturating_sub(1),
            vec![
                // valid transfer
                system_transaction::transfer(&keypair1, &keypair2.pubkey(), 2, blockhash),
                // unfunded fee-payer, successful no-op
                system_transaction::transfer(
                    &missing_keypair,
                    &missing_keypair2.pubkey(),
                    2,
                    blockhash,
                ),
                // invalid blockhash
                system_transaction::transfer(&keypair1, &keypair2.pubkey(), 2, Hash::new_unique()),
            ],
        );
        entries_to_test_shreds(
            &[entry],
            slot,
            slot.saturating_sub(1), // parent_slot
            false,                  // is_full_slot
            0,                      // version
        )
    });

    assert_matches!(
        res,
        Err(BlockstoreProcessorError::InvalidTransaction(
            TransactionError::BlockhashNotFound
        ))
    );
}

#[test]
fn test_dead_fork_entry_verification_failure() {
    let keypair2 = Keypair::new();
    let res = check_dead_fork(|genesis_keypair, bank| {
        let blockhash = bank.last_blockhash();
        let slot = bank.slot();
        let bad_hash = hash(&[2; 30]);
        let hashes_per_tick = bank.hashes_per_tick().unwrap_or(0);
        let entry = entry::next_entry(
            // Use wrong blockhash so that the entry causes an entry verification failure
            &bad_hash,
            hashes_per_tick.saturating_sub(1),
            vec![system_transaction::transfer(
                genesis_keypair,
                &keypair2.pubkey(),
                2,
                blockhash,
            )],
        );
        entries_to_test_shreds(
            &[entry],
            slot,
            slot.saturating_sub(1), // parent_slot
            false,                  // is_full_slot
            0,                      // version
        )
    });

    if let Err(BlockstoreProcessorError::InvalidBlock(block_error)) = res {
        assert_eq!(block_error, BlockError::InvalidEntryHash);
    } else {
        panic!();
    }
}

#[test]
fn test_dead_fork_invalid_tick_hash_count() {
    let res = check_dead_fork(|_keypair, bank| {
        let blockhash = bank.last_blockhash();
        let slot = bank.slot();
        let hashes_per_tick = bank.hashes_per_tick().unwrap_or(0);
        assert!(hashes_per_tick > 0);

        let too_few_hashes_tick = Entry::new(&blockhash, hashes_per_tick - 1, vec![]);
        entries_to_test_shreds(
            &[too_few_hashes_tick],
            slot,
            slot.saturating_sub(1),
            false,
            0,
        )
    });

    if let Err(BlockstoreProcessorError::InvalidBlock(block_error)) = res {
        assert_eq!(block_error, BlockError::InvalidTickHashCount);
    } else {
        panic!();
    }
}

#[test]
fn test_dead_fork_invalid_slot_tick_count() {
    agave_logger::setup();
    // Too many ticks per slot
    let res = check_dead_fork(|_keypair, bank| {
        let blockhash = bank.last_blockhash();
        let slot = bank.slot();
        let hashes_per_tick = bank.hashes_per_tick().unwrap_or(0);
        entries_to_test_shreds(
            &entry::create_ticks(bank.ticks_per_slot() + 1, hashes_per_tick, blockhash),
            slot,
            slot.saturating_sub(1),
            false,
            0,
        )
    });

    if let Err(BlockstoreProcessorError::InvalidBlock(block_error)) = res {
        assert_eq!(block_error, BlockError::TooManyTicks);
    } else {
        panic!();
    }

    // Too few ticks per slot
    let res = check_dead_fork(|_keypair, bank| {
        let blockhash = bank.last_blockhash();
        let slot = bank.slot();
        let hashes_per_tick = bank.hashes_per_tick().unwrap_or(0);
        entries_to_test_shreds(
            &entry::create_ticks(bank.ticks_per_slot() - 1, hashes_per_tick, blockhash),
            slot,
            slot.saturating_sub(1),
            true,
            0,
        )
    });

    if let Err(BlockstoreProcessorError::InvalidBlock(block_error)) = res {
        assert_eq!(block_error, BlockError::TooFewTicks);
    } else {
        panic!();
    }
}

#[test]
fn test_dead_fork_invalid_last_tick() {
    let res = check_dead_fork(|_keypair, bank| {
        let blockhash = bank.last_blockhash();
        let slot = bank.slot();
        let hashes_per_tick = bank.hashes_per_tick().unwrap_or(0);
        entries_to_test_shreds(
            &entry::create_ticks(bank.ticks_per_slot(), hashes_per_tick, blockhash),
            slot,
            slot.saturating_sub(1),
            false,
            0,
        )
    });

    if let Err(BlockstoreProcessorError::InvalidBlock(block_error)) = res {
        assert_eq!(block_error, BlockError::InvalidLastTick);
    } else {
        panic!();
    }
}

#[test]
fn test_dead_fork_trailing_entry() {
    let keypair = Keypair::new();
    let res = check_dead_fork(|funded_keypair, bank| {
        let blockhash = bank.last_blockhash();
        let slot = bank.slot();
        let hashes_per_tick = bank.hashes_per_tick().unwrap_or(0);
        let mut entries = entry::create_ticks(bank.ticks_per_slot(), hashes_per_tick, blockhash);
        let last_entry_hash = entries.last().unwrap().hash;
        let tx = system_transaction::transfer(funded_keypair, &keypair.pubkey(), 2, blockhash);
        let trailing_entry = entry::next_entry(&last_entry_hash, 1, vec![tx]);
        entries.push(trailing_entry);
        entries_to_test_shreds(
            &entries,
            slot,
            slot.saturating_sub(1), // parent_slot
            true,                   // is_full_slot
            0,                      // version
        )
    });

    if let Err(BlockstoreProcessorError::InvalidBlock(block_error)) = res {
        assert_eq!(block_error, BlockError::TrailingEntry);
    } else {
        panic!();
    }
}

#[test]
fn test_dead_fork_entry_deserialize_failure() {
    // Insert entry that causes deserialization failure
    let res = check_dead_fork(|_, bank| {
        let gibberish = [0xa5u8; 1024];

        let shredder = Shredder::new(bank.slot(), bank.parent_slot(), 0, 0).unwrap();
        let keypair = Keypair::new();
        let reed_solomon_cache = ReedSolomonCache::default();

        shredder
            .make_shreds_from_data_slice(
                &keypair,
                &gibberish,
                true,
                Hash::default(),
                0,
                0,
                &reed_solomon_cache,
                &mut ProcessShredsStats::default(),
            )
            .unwrap()
            .collect()
    });

    assert_matches!(
        res,
        Err(BlockstoreProcessorError::FailedToLoadEntries(
            BlockstoreError::InvalidShredData(_)
        ),)
    );
}

struct SlotStatusNotifierForTest {
    dead_slots: Arc<Mutex<HashSet<Slot>>>,
}

impl SlotStatusNotifierForTest {
    pub fn new(dead_slots: Arc<Mutex<HashSet<Slot>>>) -> Self {
        Self { dead_slots }
    }
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
    let (replay_vote_sender, _replay_vote_receiver) = bounded(1024);
    let (finalization_cert_sender, _finalization_cert_receiver) = bounded(1024);
    let process_active_banks_context = ProcessActiveBanksContext::new_for_tests(
        bank_forks.clone(),
        blockstore.clone(),
        replay_vote_sender,
    );
    let finish_verify = match failure {
        CompleteBankFailure::ReplayError => None,
        CompleteBankFailure::VerifyError => {
            let finish_verify = Arc::new(Barrier::new(2));
            process_active_banks_context.replay_tx_thread_pool.spawn({
                let finish_verify = finish_verify.clone();
                move || {
                    // stall verify so we can collect the result after replay finishes
                    finish_verify.wait();
                }
            });

            Some(finish_verify)
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

    if let Some(finish_verify) = finish_verify {
        finish_verify.wait();
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
    );

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
        Some(AsyncVerificationProgress::new()),
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
        Some(AsyncVerificationProgress::new()),
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
    );

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
    let parent_block_id = Hash::new_unique();
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
    let process_active_banks_context = ProcessActiveBanksContext::new_for_tests(
        bank_forks.clone(),
        blockstore,
        replay_vote_sender,
    );
    let update_parent = VersionedUpdateParent::V1(solana_entry::block_component::UpdateParentV1 {
        new_parent_slot: 0,
        new_parent_block_id: Hash::default(),
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
}

// Given a shred and a fatal expected error, check that replaying that shred causes causes the fork to be
// marked as dead. Returns the error for caller to verify.
fn check_dead_fork<F>(shred_to_insert: F) -> result::Result<(), BlockstoreProcessorError>
where
    F: Fn(&Keypair, Arc<Bank>) -> Vec<Shred>,
{
    let ledger_path = get_tmp_ledger_path!();
    let res = {
        let ReplayBlockstoreComponents {
            blockstore,
            vote_simulator,
            ..
        } = replay_blockstore_components(Some(tr(0)), 1, None);
        let VoteSimulator {
            mut progress,
            bank_forks,
            mut tbft_structs,
            validator_keypairs,
            ..
        } = vote_simulator;

        let bank0 = bank_forks.read().unwrap().get(0).unwrap();
        assert!(bank0.is_frozen());
        assert_eq!(bank0.tick_height(), bank0.max_tick_height());
        bank_forks.write().unwrap().install_scheduler_pool(
            DefaultSchedulerPool::new_for_verification(None, None, None, None, None),
        );
        let bank1 = Bank::new_from_parent(bank0, SlotLeader::default(), 1);
        bank_forks.write().unwrap().insert(bank1);
        let bank1 = bank_forks.read().unwrap().get_with_scheduler(1).unwrap();
        let bank1_progress = progress
            .entry(bank1.slot())
            .or_insert_with(|| ForkProgress::new(bank1.last_blockhash(), None, None, 0, 0, None));
        let shreds = shred_to_insert(
            &validator_keypairs.values().next().unwrap().node_keypair,
            bank1.clone(),
        );
        blockstore.insert_shreds(shreds, false).unwrap();
        let block_commitment_cache = Arc::new(RwLock::new(BlockCommitmentCache::default()));
        let exit = Arc::new(AtomicBool::new(false));
        let (replay_vote_sender, _replay_vote_receiver) = bounded(1024);
        let (finalization_cert_sender, _finalization_cert_receiver) = bounded(1024);
        let process_active_banks_context = ProcessActiveBanksContext::new_for_tests(
            bank_forks.clone(),
            blockstore.clone(),
            replay_vote_sender,
        );
        let cluster_info = cluster_info_for_tests();
        let res = ReplayStage::replay_blockstore_into_bank(
            cluster_info.my_shred_version(),
            &process_active_banks_context,
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
            // Transaction errors from the unified scheduler surface when waiting for its
            // completion, like replay stage does before freezing the bank.
            if let Some((result, _timings)) = bank1.wait_for_completed_scheduler() {
                result?;
            }
            Ok(replay_tx_count)
        });
        let max_complete_transaction_status_slot = Arc::new(AtomicU64::default());
        let rpc_subscriptions = Arc::new(RpcSubscriptions::new_for_tests(
            exit,
            max_complete_transaction_status_slot,
            bank_forks.clone(),
            block_commitment_cache,
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks),
        ));
        let (replay_vote_sender, replay_vote_receiver) = bounded(1024);
        let dead_slots = Arc::new(Mutex::new(HashSet::default()));

        let slot_status_notifier: Option<SlotStatusNotifier> = Some(Arc::new(RwLock::new(
            SlotStatusNotifierForTest::new(dead_slots.clone()),
        )));

        let rpc_subscriptions = Some(rpc_subscriptions);

        if let Err(err) = &res {
            let mut duplicate_slots_to_repair = DuplicateSlotsToRepair::default();
            let mut purge_repair_slot_counter = PurgeRepairSlotCounter::default();
            let mut dead_slot_context = DeadSlotContext {
                notifications: DeadSlotNotifications {
                    blockstore: blockstore.clone(),
                    rpc_subscriptions: rpc_subscriptions.clone(),
                    slot_status_notifier: slot_status_notifier.clone(),
                    replay_vote_sender: replay_vote_sender.clone(),
                },
                duplicate: DeadSlotDuplicateContext {
                    root: 0,
                    duplicate_slots_to_repair: &mut duplicate_slots_to_repair,
                    ancestor_hashes_replay_update_sender: &process_active_banks_context
                        .ancestor_hashes_replay_update_sender,
                    purge_repair_slot_counter: &mut purge_repair_slot_counter,
                    tbft_structs: Some(&mut tbft_structs),
                },
                migration_status: process_active_banks_context.migration_status.as_ref(),
            };
            mark_replay_dead_slot(&bank1, err, &mut progress, &mut dead_slot_context);
        }
        assert_eq!(
            replay_vote_receiver.try_recv(),
            Ok(ReplayVoteMessage::InvalidBank {
                replay_bank_id: bank1.bank_id(),
                replay_slot: bank1.slot(),
            })
        );
        assert!(dead_slots.lock().unwrap().contains(&bank1.slot()));
        // Check that the erroring bank was marked as dead in the progress map
        assert!(
            progress
                .get(&bank1.slot())
                .map(|b| b.dead_reason.is_some())
                .unwrap_or(false)
        );

        // Check that the erroring bank was marked as dead in blockstore
        assert!(blockstore.is_dead(bank1.slot()));
        res.map(|_| ())
    };
    let _ignored = remove_dir_all(ledger_path);
    res
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
fn test_compute_bank_stats_confirmed() {
    let vote_keypairs = ValidatorVoteKeypairs::new_rand();
    let my_node_pubkey = vote_keypairs.node_keypair.pubkey();
    let my_vote_pubkey = vote_keypairs.vote_keypair.pubkey();
    let keypairs: HashMap<_, _> = vec![(my_node_pubkey, vote_keypairs)].into_iter().collect();

    let (bank_forks, mut progress, mut heaviest_subtree_fork_choice) =
        vote_simulator::initialize_state(&keypairs, 10_000);
    let mut latest_validator_votes_for_frozen_banks = LatestValidatorVotesForFrozenBanks::default();
    let bank0 = bank_forks.read().unwrap().get(0).unwrap();
    let my_keypairs = keypairs.get(&my_node_pubkey).unwrap();
    let tower_sync = TowerSync::new_from_slots(vec![0], bank0.hash(), None);
    let vote_tx = vote_transaction::new_tower_sync_transaction(
        tower_sync,
        bank0.last_blockhash(),
        &my_keypairs.node_keypair,
        &my_keypairs.vote_keypair,
        &my_keypairs.vote_keypair,
        None,
    );
    let mut vote_slots = HashSet::default();

    // Test confirmations
    let ancestors = bank_forks.read().unwrap().ancestors();
    let mut frozen_banks: Vec<_> = bank_forks
        .read()
        .unwrap()
        .frozen_banks()
        .map(|(_slot, bank)| bank)
        .collect();
    let mut tower = Tower::new_for_tests(0, 0.67);
    let newly_computed = ReplayStage::compute_bank_stats(
        &my_vote_pubkey,
        &ancestors,
        &mut frozen_banks,
        &mut tower,
        &mut progress,
        &VoteTracker::default(),
        &ClusterSlots::default_for_tests(),
        &bank_forks,
        &mut heaviest_subtree_fork_choice,
        &mut latest_validator_votes_for_frozen_banks,
        &mut vote_slots,
        &MigrationStatus::default(),
    );

    // bank 0 has no votes, should not send any votes on the channel
    assert_eq!(newly_computed, vec![0]);
    // The only vote is in bank 1, and bank_forks does not currently contain
    // bank 1, so no slot should be confirmed.
    {
        let fork_progress = progress.get(&0).unwrap();
        let confirmed_forks = ReplayStage::tower_duplicate_confirmed_forks(
            &tower,
            &fork_progress.fork_stats.voted_stakes,
            fork_progress.fork_stats.total_stake,
            &progress,
            &bank_forks,
        );

        assert!(confirmed_forks.is_empty());
    }

    let bank1 = Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        bank0.clone(),
        SlotLeader {
            id: my_node_pubkey,
            vote_address: my_vote_pubkey,
        },
        1,
    );
    bank1.process_transaction(&vote_tx).unwrap();
    bank1.freeze();

    // Insert the bank that contains a vote for slot 0, which confirms slot 0
    progress.insert(
        1,
        ForkProgress::new(bank0.last_blockhash(), None, None, 0, 0, None),
    );
    let ancestors = bank_forks.read().unwrap().ancestors();
    let mut frozen_banks: Vec<_> = bank_forks
        .read()
        .unwrap()
        .frozen_banks()
        .map(|(_slot, bank)| bank)
        .collect();
    let newly_computed = ReplayStage::compute_bank_stats(
        &my_vote_pubkey,
        &ancestors,
        &mut frozen_banks,
        &mut tower,
        &mut progress,
        &VoteTracker::default(),
        &ClusterSlots::default_for_tests(),
        &bank_forks,
        &mut heaviest_subtree_fork_choice,
        &mut latest_validator_votes_for_frozen_banks,
        &mut vote_slots,
        &MigrationStatus::default(),
    );

    // Bank 1 had one vote
    assert_eq!(newly_computed, vec![1]);
    {
        let fork_progress = progress.get(&1).unwrap();
        let confirmed_forks = ReplayStage::tower_duplicate_confirmed_forks(
            &tower,
            &fork_progress.fork_stats.voted_stakes,
            fork_progress.fork_stats.total_stake,
            &progress,
            &bank_forks,
        );
        // No new stats should have been computed
        assert_eq!(confirmed_forks, vec![(0, bank0.hash())]);
    }

    let ancestors = bank_forks.read().unwrap().ancestors();
    let mut frozen_banks: Vec<_> = bank_forks
        .read()
        .unwrap()
        .frozen_banks()
        .map(|(_slot, bank)| bank)
        .collect();
    let newly_computed = ReplayStage::compute_bank_stats(
        &my_vote_pubkey,
        &ancestors,
        &mut frozen_banks,
        &mut tower,
        &mut progress,
        &VoteTracker::default(),
        &ClusterSlots::default_for_tests(),
        &bank_forks,
        &mut heaviest_subtree_fork_choice,
        &mut latest_validator_votes_for_frozen_banks,
        &mut vote_slots,
        &MigrationStatus::default(),
    );
    // No new stats should have been computed
    assert!(newly_computed.is_empty());
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
fn test_child_bank_heavier() {
    // Init state
    let mut vote_simulator = VoteSimulator::new(1);
    let my_node_pubkey = vote_simulator.node_pubkeys[0];
    let mut tower = Tower::default();

    // Create the tree of banks in a BankForks object
    let forks = tr(0) / (tr(1) / (tr(2) / (tr(3))));

    // Set the voting behavior
    let mut cluster_votes = HashMap::new();
    let votes = vec![2];
    cluster_votes.insert(my_node_pubkey, votes.clone());
    vote_simulator.fill_bank_forks(forks, &cluster_votes, true);
    let mut vote_slots = HashSet::default();

    // Fill banks with votes
    for vote in votes {
        assert!(
            vote_simulator
                .simulate_vote(vote, &my_node_pubkey, &mut tower,)
                .is_empty()
        );
    }

    let mut frozen_banks: Vec<_> = vote_simulator
        .bank_forks
        .read()
        .unwrap()
        .frozen_banks()
        .map(|(_slot, bank)| bank)
        .collect();

    let my_vote_pubkey = vote_simulator.vote_pubkeys[0];
    ReplayStage::compute_bank_stats(
        &my_vote_pubkey,
        &vote_simulator.bank_forks.read().unwrap().ancestors(),
        &mut frozen_banks,
        &mut tower,
        &mut vote_simulator.progress,
        &VoteTracker::default(),
        &ClusterSlots::default_for_tests(),
        &vote_simulator.bank_forks,
        &mut vote_simulator.tbft_structs.heaviest_subtree_fork_choice,
        &mut vote_simulator.latest_validator_votes_for_frozen_banks,
        &mut vote_slots,
        &MigrationStatus::default(),
    );

    frozen_banks.sort_by_key(|bank| bank.slot());
    for pair in frozen_banks.windows(2) {
        let first = vote_simulator
            .progress
            .get_fork_stats(pair[0].slot())
            .unwrap()
            .fork_weight();
        let second = vote_simulator
            .progress
            .get_fork_stats(pair[1].slot())
            .unwrap()
            .fork_weight();
        assert!(second >= first);
    }
    for bank in frozen_banks {
        // The only leaf should always be chosen over parents
        assert_eq!(
            vote_simulator
                .tbft_structs
                .heaviest_subtree_fork_choice
                .best_slot(&(bank.slot(), bank.hash()))
                .unwrap()
                .0,
            3
        );
    }
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
fn test_purge_unconfirmed_duplicate_slot() {
    let (vote_simulator, blockstore) = setup_default_forks(2, None::<GenerateVotes>);
    let VoteSimulator {
        bank_forks,
        node_pubkeys,
        mut progress,
        validator_keypairs,
        ..
    } = vote_simulator;

    // Create bank 7
    let root_bank = bank_forks.read().unwrap().root_bank();
    let bank7 = Bank::new_from_parent(
        bank_forks.read().unwrap().get(6).unwrap(),
        SlotLeader::default(),
        7,
    );
    bank_forks.write().unwrap().insert(bank7);
    blockstore.add_tree(tr(6) / tr(7), false, false, 3, Hash::default());
    let bank7 = bank_forks.read().unwrap().get(7).unwrap();
    let mut descendants = bank_forks.read().unwrap().descendants();
    let mut ancestors = bank_forks.read().unwrap().ancestors();

    // Process a transfer on bank 7
    let sender = node_pubkeys[0];
    let receiver = node_pubkeys[1];
    let old_balance = bank7.get_balance(&sender);
    let transfer_amount = old_balance / 2;
    let transfer_sig = bank7
        .transfer(
            transfer_amount,
            &validator_keypairs.get(&sender).unwrap().node_keypair,
            &receiver,
        )
        .unwrap();

    // Process a vote for slot 0 in bank 5
    let validator0_keypairs = &validator_keypairs.get(&sender).unwrap();
    let bank0 = bank_forks.read().unwrap().get(0).unwrap();
    let tower_sync = TowerSync::new_from_slots(vec![0], bank0.hash(), None);
    let vote_tx = vote_transaction::new_tower_sync_transaction(
        tower_sync,
        bank0.last_blockhash(),
        &validator0_keypairs.node_keypair,
        &validator0_keypairs.vote_keypair,
        &validator0_keypairs.vote_keypair,
        None,
    );
    bank7.process_transaction(&vote_tx).unwrap();
    assert!(bank7.get_signature_status(&vote_tx.signatures[0]).is_some());

    // Both signatures should exist in status cache
    assert!(bank7.get_signature_status(&vote_tx.signatures[0]).is_some());
    assert!(bank7.get_signature_status(&transfer_sig).is_some());

    // Give all slots a bank hash but mark slot 7 dead
    for i in 0..=6 {
        blockstore.insert_bank_hash(i, Hash::new_unique(), false);
    }
    blockstore
        .set_dead_slot(7)
        .expect("Failed to mark slot as dead in blockstore");

    // Purging slot 5 should purge only slots 5 and its descendant 6. Since 7 is already dead,
    // it gets reset but not removed
    ReplayStage::purge_unconfirmed_slot(
        5,
        &mut ancestors,
        &mut descendants,
        &mut progress,
        &root_bank,
        &bank_forks,
        &blockstore,
    );
    for i in 5..=7 {
        assert!(bank_forks.read().unwrap().get(i).is_none());
        assert!(progress.get(&i).is_none());
    }
    for i in 0..=4 {
        assert!(bank_forks.read().unwrap().get(i).is_some());
        assert!(progress.get(&i).is_some());
    }

    // Blockstore should have been cleared
    for slot in &[5, 6] {
        assert!(!blockstore.is_full(*slot));
        assert!(!blockstore.is_dead(*slot));
        assert!(blockstore.get_slot_entries(*slot, 0).unwrap().is_empty());
    }

    // Slot 7 was marked dead before, should no longer be marked
    assert!(!blockstore.is_dead(7));
    assert!(!blockstore.get_slot_entries(7, 0).unwrap().is_empty());

    // Should not be able to find signature in slot 5 for previously
    // processed transactions
    assert!(bank7.get_signature_status(&vote_tx.signatures[0]).is_none());
    assert!(bank7.get_signature_status(&transfer_sig).is_none());

    // Getting balance should return the old balance (accounts were cleared)
    assert_eq!(bank7.get_balance(&sender), old_balance);

    // Purging slot 4 should purge only slot 4
    let mut descendants = bank_forks.read().unwrap().descendants();
    let mut ancestors = bank_forks.read().unwrap().ancestors();
    ReplayStage::purge_unconfirmed_slot(
        4,
        &mut ancestors,
        &mut descendants,
        &mut progress,
        &root_bank,
        &bank_forks,
        &blockstore,
    );
    for i in 4..=6 {
        assert!(bank_forks.read().unwrap().get(i).is_none());
        assert!(progress.get(&i).is_none());
        assert!(blockstore.get_slot_entries(i, 0).unwrap().is_empty());
    }
    for i in 0..=3 {
        assert!(bank_forks.read().unwrap().get(i).is_some());
        assert!(progress.get(&i).is_some());
        assert!(!blockstore.get_slot_entries(i, 0).unwrap().is_empty());
    }

    // Purging slot 1 should purge both forks 2 and 3 but leave 7 untouched as it is dead
    let mut descendants = bank_forks.read().unwrap().descendants();
    let mut ancestors = bank_forks.read().unwrap().ancestors();
    ReplayStage::purge_unconfirmed_slot(
        1,
        &mut ancestors,
        &mut descendants,
        &mut progress,
        &root_bank,
        &bank_forks,
        &blockstore,
    );
    for i in 1..=6 {
        assert!(bank_forks.read().unwrap().get(i).is_none());
        assert!(progress.get(&i).is_none());
        assert!(blockstore.get_slot_entries(i, 0).unwrap().is_empty());
    }
    assert!(bank_forks.read().unwrap().get(0).is_some());
    assert!(progress.get(&0).is_some());

    // Slot 7 untouched
    assert!(!blockstore.is_dead(7));
    assert!(!blockstore.get_slot_entries(7, 0).unwrap().is_empty());
}

#[test]
fn test_purge_unconfirmed_duplicate_slots_and_reattach() {
    let ReplayBlockstoreComponents {
        blockstore,
        validator_node_to_vote_keys,
        vote_simulator,
        leader_schedule_cache,
        rpc_subscriptions,
        ..
    } = replay_blockstore_components(
        Some(tr(0) / (tr(1) / (tr(2) / (tr(4))) / (tr(3) / (tr(5) / (tr(6)))))),
        1,
        None::<GenerateVotes>,
    );

    let VoteSimulator {
        bank_forks,
        mut progress,
        ..
    } = vote_simulator;

    let mut replay_timing = ReplayLoopTiming::default();

    // Create bank 7 and insert to blockstore and bank forks
    let root_bank = bank_forks.read().unwrap().root_bank();
    let bank7 = Bank::new_from_parent(
        bank_forks.read().unwrap().get(6).unwrap(),
        SlotLeader::default(),
        7,
    );
    bank_forks.write().unwrap().insert(bank7);
    blockstore.add_tree(tr(6) / tr(7), false, false, 3, Hash::default());
    let mut descendants = bank_forks.read().unwrap().descendants();
    let mut ancestors = bank_forks.read().unwrap().ancestors();

    // Mark earlier slots as frozen, but we have the wrong version of slots 3 and 5, so slot 6 is dead and
    // slot 7 is unreplayed
    for i in 0..=5 {
        blockstore.insert_bank_hash(i, Hash::new_unique(), false);
    }
    blockstore
        .set_dead_slot(6)
        .expect("Failed to mark slot 6 as dead in blockstore");

    // Purge slot 3 as it is duplicate, this should also purge slot 5 but not touch 6 and 7
    ReplayStage::purge_unconfirmed_slot(
        3,
        &mut ancestors,
        &mut descendants,
        &mut progress,
        &root_bank,
        &bank_forks,
        &blockstore,
    );
    for slot in &[3, 5, 6, 7] {
        assert!(bank_forks.read().unwrap().get(*slot).is_none());
        assert!(progress.get(slot).is_none());
    }
    for slot in &[3, 5] {
        assert!(!blockstore.is_full(*slot));
        assert!(!blockstore.is_dead(*slot));
        assert!(blockstore.get_slot_entries(*slot, 0).unwrap().is_empty());
    }
    for slot in 6..=7 {
        assert!(!blockstore.is_dead(slot));
        assert!(!blockstore.get_slot_entries(slot, 0).unwrap().is_empty())
    }

    // Simulate repair fixing slot 3 and 5
    let (shreds, _) = make_slot_entries(
        3, // slot
        1, // parent_slot
        8, // num_entries
    );
    blockstore.insert_shreds(shreds, false).unwrap();
    let (shreds, _) = make_slot_entries(
        5, // slot
        3, // parent_slot
        8, // num_entries
    );
    blockstore.insert_shreds(shreds, false).unwrap();

    let rpc_subscriptions = Some(rpc_subscriptions);

    // 3 should now be an active bank
    ReplayStage::generate_new_bank_forks(
        NewBankForksContext {
            blockstore: &blockstore,
            bank_forks: &bank_forks,
            leader_schedule_cache: &leader_schedule_cache,
            rpc_subscriptions: rpc_subscriptions.as_deref(),
            slot_status_notifier: &None,
            migration_status: &MigrationStatus::default(),
            my_pubkey: &Pubkey::default(),
        },
        &mut progress,
        &mut replay_timing,
    );
    assert_eq!(bank_forks.read().unwrap().active_bank_slots(), vec![3]);

    // Freeze 3
    {
        let bank3 = bank_forks.read().unwrap().get(3).unwrap();
        progress.insert(
            3,
            ForkProgress::new_from_bank(
                &bank3,
                bank3.leader_id(),
                validator_node_to_vote_keys.get(bank3.leader_id()).unwrap(),
                Some(1),
                0,
                0,
                None,
            ),
        );
        bank3.freeze();
    }
    // 5 Should now be an active bank
    ReplayStage::generate_new_bank_forks(
        NewBankForksContext {
            blockstore: &blockstore,
            bank_forks: &bank_forks,
            leader_schedule_cache: &leader_schedule_cache,
            rpc_subscriptions: rpc_subscriptions.as_deref(),
            slot_status_notifier: &None,
            migration_status: &MigrationStatus::default(),
            my_pubkey: &Pubkey::default(),
        },
        &mut progress,
        &mut replay_timing,
    );
    assert_eq!(bank_forks.read().unwrap().active_bank_slots(), vec![5]);

    // Freeze 5
    {
        let bank5 = bank_forks.read().unwrap().get(5).unwrap();
        progress.insert(
            5,
            ForkProgress::new_from_bank(
                &bank5,
                bank5.leader_id(),
                validator_node_to_vote_keys.get(bank5.leader_id()).unwrap(),
                Some(3),
                0,
                0,
                None,
            ),
        );
        bank5.freeze();
    }
    // 6 should now be an active bank even though we haven't repaired it because it
    // wasn't dumped
    ReplayStage::generate_new_bank_forks(
        NewBankForksContext {
            blockstore: &blockstore,
            bank_forks: &bank_forks,
            leader_schedule_cache: &leader_schedule_cache,
            rpc_subscriptions: rpc_subscriptions.as_deref(),
            slot_status_notifier: &None,
            migration_status: &MigrationStatus::default(),
            my_pubkey: &Pubkey::default(),
        },
        &mut progress,
        &mut replay_timing,
    );
    assert_eq!(bank_forks.read().unwrap().active_bank_slots(), vec![6]);

    // Freeze 6 now that we have the correct version of 5.
    {
        let bank6 = bank_forks.read().unwrap().get(6).unwrap();
        progress.insert(
            6,
            ForkProgress::new_from_bank(
                &bank6,
                bank6.leader_id(),
                validator_node_to_vote_keys.get(bank6.leader_id()).unwrap(),
                Some(5),
                0,
                0,
                None,
            ),
        );
        bank6.freeze();
    }
    // 7 should be found as an active bank
    ReplayStage::generate_new_bank_forks(
        NewBankForksContext {
            blockstore: &blockstore,
            bank_forks: &bank_forks,
            leader_schedule_cache: &leader_schedule_cache,
            rpc_subscriptions: rpc_subscriptions.as_deref(),
            slot_status_notifier: &None,
            migration_status: &MigrationStatus::default(),
            my_pubkey: &Pubkey::default(),
        },
        &mut progress,
        &mut replay_timing,
    );
    assert_eq!(bank_forks.read().unwrap().active_bank_slots(), vec![7]);
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
    // Slot 8: 40 shreds, replay_fec_set_index=32; 40 >= 32 so skipped.
    for (slot, shreds) in [(4, 5), (8, 40)] {
        let bank = Bank::new_from_parent(bank0.clone(), SlotLeader::default(), slot);
        bank_forks.write().unwrap().insert(bank);
        let p = ForkProgress::new(Hash::default(), Some(0), None, 0, 0, None);
        p.replay_progress.write().unwrap().num_shreds = shreds;
        progress.insert(slot, p);
    }

    let (tx, rx) = bounded(1024);
    for slot in [4, 8] {
        let parent_block_id = Hash::new_unique();
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
    );

    assert!(progress.get(&4).is_none()); // cleared: 5 < 32
    assert!(progress.get(&8).is_some()); // skipped: 40 >= 32
    assert_eq!(
        replay_vote_receiver.try_recv(),
        Ok(ReplayVoteMessage::InvalidBank {
            replay_bank_id: cleared_bank_id,
            replay_slot: 4,
        })
    );
    assert!(replay_vote_receiver.try_recv().is_err());
}

#[test]
fn test_headerless_update_parent() {
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
        new_parent_block_id: Hash::default(),
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

    let mut replay_timing = ReplayLoopTiming::default();
    ReplayStage::generate_new_bank_forks(
        NewBankForksContext {
            blockstore: &blockstore,
            bank_forks: &bank_forks,
            leader_schedule_cache: &leader_schedule_cache,
            rpc_subscriptions: None,
            slot_status_notifier: &None,
            migration_status: &migration_status,
            my_pubkey: &my_pubkey,
        },
        &mut progress,
        &mut replay_timing,
    );

    assert!(
        bank_forks.read().unwrap().get(slot).is_some(),
        "headerless UpdateParent should create a replay bank from the marker"
    );
    assert_eq!(
        progress
            .get(&slot)
            .unwrap()
            .replay_progress
            .read()
            .unwrap()
            .num_shreds,
        32
    );
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
    );

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
    );

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
    );

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
        new_parent_block_id: Hash::default(),
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
    let parent_block_id = Hash::new_unique();
    insert_update_parent_slot(&blockstore, slot, 3, 0, parent_block_id, 32);
    let bank0 = bank_forks.read().unwrap().get(0).unwrap();
    let bank = Bank::new_from_parent(bank0, SlotLeader::default(), slot);
    bank_forks.write().unwrap().insert(bank);

    let mut p = ForkProgress::new(Hash::default(), Some(0), None, 0, 0, None);
    p.replay_progress.write().unwrap().num_shreds = 5;
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
    );

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
            parent_block: Block {
                slot: 7,
                block_id: Hash::new_unique(),
            },
            block_timer: Instant::now(),
        })
        .unwrap();

    ReplayStage::try_send_latest_optimistic_parent(
        &sender,
        &receiver,
        LeaderWindowInfo {
            start_slot: 12,
            end_slot: 15,
            parent_block: Block {
                slot: 11,
                block_id: Hash::new_unique(),
            },
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
            parent_block: Block {
                slot: 19,
                block_id: Hash::new_unique(),
            },
            block_timer: Instant::now(),
        })
        .unwrap();

    ReplayStage::try_send_latest_optimistic_parent(
        &sender,
        &receiver,
        LeaderWindowInfo {
            start_slot: 20,
            end_slot: 23,
            parent_block: Block {
                slot: 19,
                block_id: Hash::new_unique(),
            },
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
            parent_block: Block {
                slot: 19,
                block_id: Hash::new_unique(),
            },
            block_timer: Instant::now(),
        })
        .unwrap();

    ReplayStage::try_send_latest_optimistic_parent(
        &sender,
        &receiver,
        LeaderWindowInfo {
            start_slot: 16,
            end_slot: 19,
            parent_block: Block {
                slot: 15,
                block_id: Hash::new_unique(),
            },
            block_timer: Instant::now(),
        },
    );

    let latest = receiver.try_recv().unwrap();
    assert_eq!(latest.start_slot, 20);
    assert!(receiver.try_recv().is_err());
}

#[test]
fn test_skip_own_update_full() {
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
        Hash::new_unique(),
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
            migration_status: &migration_status,
            my_pubkey: &my_pubkey,
        },
        &mut progress,
        &mut replay_timing,
    );

    assert!(
        bank_forks.read().unwrap().get(slot).is_none(),
        "live replay must not create own leader banks from blockstore"
    );
    assert!(progress.get(&slot).is_none());
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
fn test_unconfirmed_duplicate_slots_and_lockouts_for_non_heaviest_fork() {
    /*
        Build fork structure:

             slot 0
               |
             slot 1
             /    \
        slot 2    |
           |      |
        slot 3    |
           |      |
        slot 4    |
                slot 5
    */
    let forks = tr(0) / (tr(1) / (tr(2) / (tr(3) / (tr(4)))) / tr(5));

    let mut vote_simulator = VoteSimulator::new(1);
    vote_simulator.fill_bank_forks(forks, &HashMap::<Pubkey, Vec<u64>>::new(), true);
    let (bank_forks, mut progress) = (vote_simulator.bank_forks, vote_simulator.progress);
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Arc::new(
        Blockstore::open(ledger_path.path()).expect("Expected to be able to open database ledger"),
    );
    let mut tower = Tower::new_for_tests(8, 2.0 / 3.0);
    let mut vote_slots = HashSet::default();

    // All forks have same weight so heaviest bank to vote/reset on should be the tip of
    // the fork with the lower slot
    let (vote_fork, reset_fork, _) = run_compute_and_select_forks(
        &bank_forks,
        &mut progress,
        &mut tower,
        &mut vote_simulator.tbft_structs.heaviest_subtree_fork_choice,
        &mut vote_simulator.latest_validator_votes_for_frozen_banks,
        None,
        &mut vote_slots,
    );
    assert_eq!(vote_fork.unwrap(), 4);
    assert_eq!(reset_fork.unwrap(), 4);

    // Record the vote for 5 which is not on the heaviest fork.
    tower.record_bank_vote(&bank_forks.read().unwrap().get(5).unwrap());

    // 4 should be the heaviest slot, but should not be votable
    // because of lockout. 5 is the heaviest slot on the same fork as the last vote.
    let (vote_fork, reset_fork, heaviest_fork_failures) = run_compute_and_select_forks(
        &bank_forks,
        &mut progress,
        &mut tower,
        &mut vote_simulator.tbft_structs.heaviest_subtree_fork_choice,
        &mut vote_simulator.latest_validator_votes_for_frozen_banks,
        None,
        &mut vote_slots,
    );
    assert!(vote_fork.is_none());
    assert_eq!(reset_fork, Some(5));
    assert_eq!(
        heaviest_fork_failures,
        vec![
            HeaviestForkFailures::FailedSwitchThreshold(4, 0, 10000),
            HeaviestForkFailures::LockedOut(4)
        ]
    );

    // Mark 5 as duplicate
    blockstore.store_duplicate_slot(5, vec![], vec![]).unwrap();
    let mut duplicate_slots_tracker = DuplicateSlotsTracker::default();
    let mut purge_repair_slot_counter = PurgeRepairSlotCounter::default();
    let mut duplicate_confirmed_slots = DuplicateConfirmedSlots::default();
    let mut epoch_slots_frozen_slots = EpochSlotsFrozenSlots::default();
    let bank5_hash = bank_forks.read().unwrap().bank_hash(5).unwrap();
    assert_ne!(bank5_hash, Hash::default());
    let duplicate_state = DuplicateState::new_from_state(
        5,
        &duplicate_confirmed_slots,
        &vote_simulator.tbft_structs.heaviest_subtree_fork_choice,
        || progress.is_dead(5).unwrap_or(false),
        || Some(bank5_hash),
    );
    let (ancestor_hashes_replay_update_sender, _ancestor_hashes_replay_update_receiver) =
        bounded(1024);
    check_slot_agrees_with_cluster(
        5,
        bank_forks.read().unwrap().root(),
        &blockstore,
        &mut duplicate_slots_tracker,
        &mut epoch_slots_frozen_slots,
        &mut vote_simulator.tbft_structs.heaviest_subtree_fork_choice,
        &mut DuplicateSlotsToRepair::default(),
        &ancestor_hashes_replay_update_sender,
        &mut purge_repair_slot_counter,
        SlotStateUpdate::Duplicate(duplicate_state),
    );

    // 4 should be the heaviest slot, but should not be votable
    // because of lockout. 5 is no longer valid due to it being a duplicate, however we still
    // reset onto 5.
    let (vote_fork, reset_fork, heaviest_fork_failures) = run_compute_and_select_forks(
        &bank_forks,
        &mut progress,
        &mut tower,
        &mut vote_simulator.tbft_structs.heaviest_subtree_fork_choice,
        &mut vote_simulator.latest_validator_votes_for_frozen_banks,
        None,
        &mut vote_slots,
    );
    assert!(vote_fork.is_none());
    assert_eq!(reset_fork, Some(5));
    assert_eq!(
        heaviest_fork_failures,
        vec![
            HeaviestForkFailures::FailedSwitchThreshold(4, 0, 10000),
            HeaviestForkFailures::LockedOut(4)
        ]
    );

    // Continue building on 5
    let forks = tr(5) / (tr(6) / (tr(7) / (tr(8) / (tr(9)))) / tr(10));
    vote_simulator.bank_forks = bank_forks;
    vote_simulator.progress = progress;
    vote_simulator.fill_bank_forks(forks, &HashMap::<Pubkey, Vec<u64>>::new(), true);
    let (bank_forks, mut progress) = (vote_simulator.bank_forks, vote_simulator.progress);
    // 4 is still the heaviest slot, but not votable because of lockout.
    // 9 is the deepest slot from our last voted fork (5), so it is what we should
    // reset to.
    let (vote_fork, reset_fork, heaviest_fork_failures) = run_compute_and_select_forks(
        &bank_forks,
        &mut progress,
        &mut tower,
        &mut vote_simulator.tbft_structs.heaviest_subtree_fork_choice,
        &mut vote_simulator.latest_validator_votes_for_frozen_banks,
        None,
        &mut vote_slots,
    );
    assert!(vote_fork.is_none());
    assert_eq!(reset_fork, Some(9));
    assert_eq!(
        heaviest_fork_failures,
        vec![
            HeaviestForkFailures::FailedSwitchThreshold(4, 0, 10000),
            HeaviestForkFailures::LockedOut(4)
        ]
    );

    // If slot 5 is marked as confirmed, it becomes the heaviest bank on same slot again
    let mut duplicate_slots_to_repair = DuplicateSlotsToRepair::default();
    duplicate_confirmed_slots.insert(5, bank5_hash);
    let duplicate_confirmed_state = DuplicateConfirmedState::new_from_state(
        bank5_hash,
        || progress.is_dead(5).unwrap_or(false),
        || Some(bank5_hash),
    );
    check_slot_agrees_with_cluster(
        5,
        bank_forks.read().unwrap().root(),
        &blockstore,
        &mut duplicate_slots_tracker,
        &mut epoch_slots_frozen_slots,
        &mut vote_simulator.tbft_structs.heaviest_subtree_fork_choice,
        &mut duplicate_slots_to_repair,
        &ancestor_hashes_replay_update_sender,
        &mut purge_repair_slot_counter,
        SlotStateUpdate::DuplicateConfirmed(duplicate_confirmed_state),
    );

    // The confirmed hash is detected in `progress`, which means
    // it's confirmation on the replayed block. This means we have
    // the right version of the block, so `duplicate_slots_to_repair`
    // should be empty
    assert!(duplicate_slots_to_repair.is_empty());

    // We should still reset to slot 9 as it's the heaviest on the now valid
    // fork.
    let (vote_fork, reset_fork, heaviest_fork_failures) = run_compute_and_select_forks(
        &bank_forks,
        &mut progress,
        &mut tower,
        &mut vote_simulator.tbft_structs.heaviest_subtree_fork_choice,
        &mut vote_simulator.latest_validator_votes_for_frozen_banks,
        None,
        &mut vote_slots,
    );
    assert!(vote_fork.is_none());
    assert_eq!(reset_fork.unwrap(), 9);
    assert_eq!(
        heaviest_fork_failures,
        vec![
            HeaviestForkFailures::FailedSwitchThreshold(4, 0, 10000),
            HeaviestForkFailures::LockedOut(4)
        ]
    );

    // Resetting our forks back to how it was should allow us to reset to our
    // last vote which was previously marked as invalid and now duplicate confirmed
    let bank6_hash = bank_forks.read().unwrap().bank_hash(6).unwrap();
    let _ = vote_simulator
        .tbft_structs
        .heaviest_subtree_fork_choice
        .split_off(&(6, bank6_hash));
    // Should now pick 5 as the heaviest fork from last vote again.
    let (vote_fork, reset_fork, heaviest_fork_failures) = run_compute_and_select_forks(
        &bank_forks,
        &mut progress,
        &mut tower,
        &mut vote_simulator.tbft_structs.heaviest_subtree_fork_choice,
        &mut vote_simulator.latest_validator_votes_for_frozen_banks,
        None,
        &mut vote_slots,
    );
    assert!(vote_fork.is_none());
    assert_eq!(reset_fork.unwrap(), 5);
    assert_eq!(
        heaviest_fork_failures,
        vec![
            HeaviestForkFailures::FailedSwitchThreshold(4, 0, 10000),
            HeaviestForkFailures::LockedOut(4)
        ]
    );
}

#[test]
fn test_unconfirmed_duplicate_slots_and_lockouts() {
    /*
        Build fork structure:

             slot 0
               |
             slot 1
             /    \
        slot 2    |
           |      |
        slot 3    |
           |      |
        slot 4    |
                slot 5
                  |
                slot 6
    */
    let forks = tr(0) / (tr(1) / (tr(2) / (tr(3) / (tr(4)))) / (tr(5) / (tr(6))));

    // Make enough validators for vote switch threshold later
    let mut vote_simulator = VoteSimulator::new(2);
    let validator_votes: HashMap<Pubkey, Vec<u64>> = vec![
        (vote_simulator.node_pubkeys[0], vec![5]),
        (vote_simulator.node_pubkeys[1], vec![2]),
    ]
    .into_iter()
    .collect();
    vote_simulator.fill_bank_forks(forks, &validator_votes, true);

    let (bank_forks, mut progress) = (vote_simulator.bank_forks, vote_simulator.progress);
    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Arc::new(
        Blockstore::open(ledger_path.path()).expect("Expected to be able to open database ledger"),
    );
    let mut tower = Tower::new_for_tests(8, 0.67);
    let mut vote_slots = HashSet::default();

    // All forks have same weight so heaviest bank to vote/reset on should be the tip of
    // the fork with the lower slot
    let (vote_fork, reset_fork, _) = run_compute_and_select_forks(
        &bank_forks,
        &mut progress,
        &mut tower,
        &mut vote_simulator.tbft_structs.heaviest_subtree_fork_choice,
        &mut vote_simulator.latest_validator_votes_for_frozen_banks,
        None,
        &mut vote_slots,
    );
    assert_eq!(vote_fork.unwrap(), 4);
    assert_eq!(reset_fork.unwrap(), 4);

    // Record the vote for 4
    tower.record_bank_vote(&bank_forks.read().unwrap().get(4).unwrap());

    // Mark 4 as duplicate, 3 should be the heaviest slot, but should not be votable
    // because of lockout
    blockstore.store_duplicate_slot(4, vec![], vec![]).unwrap();
    let mut duplicate_slots_tracker = DuplicateSlotsTracker::default();
    let mut duplicate_confirmed_slots = DuplicateConfirmedSlots::default();
    let mut epoch_slots_frozen_slots = EpochSlotsFrozenSlots::default();
    let bank4_hash = bank_forks.read().unwrap().bank_hash(4).unwrap();
    assert_ne!(bank4_hash, Hash::default());
    let duplicate_state = DuplicateState::new_from_state(
        4,
        &duplicate_confirmed_slots,
        &vote_simulator.tbft_structs.heaviest_subtree_fork_choice,
        || progress.is_dead(4).unwrap_or(false),
        || Some(bank4_hash),
    );
    let (ancestor_hashes_replay_update_sender, _ancestor_hashes_replay_update_receiver) =
        bounded(1024);
    check_slot_agrees_with_cluster(
        4,
        bank_forks.read().unwrap().root(),
        &blockstore,
        &mut duplicate_slots_tracker,
        &mut epoch_slots_frozen_slots,
        &mut vote_simulator.tbft_structs.heaviest_subtree_fork_choice,
        &mut DuplicateSlotsToRepair::default(),
        &ancestor_hashes_replay_update_sender,
        &mut PurgeRepairSlotCounter::default(),
        SlotStateUpdate::Duplicate(duplicate_state),
    );

    let (vote_fork, reset_fork, _) = run_compute_and_select_forks(
        &bank_forks,
        &mut progress,
        &mut tower,
        &mut vote_simulator.tbft_structs.heaviest_subtree_fork_choice,
        &mut vote_simulator.latest_validator_votes_for_frozen_banks,
        None,
        &mut vote_slots,
    );
    assert!(vote_fork.is_none());
    assert_eq!(reset_fork, Some(3));

    // Now mark 2, an ancestor of 4, as duplicate
    blockstore.store_duplicate_slot(2, vec![], vec![]).unwrap();
    let bank2_hash = bank_forks.read().unwrap().bank_hash(2).unwrap();
    assert_ne!(bank2_hash, Hash::default());
    let duplicate_state = DuplicateState::new_from_state(
        2,
        &duplicate_confirmed_slots,
        &vote_simulator.tbft_structs.heaviest_subtree_fork_choice,
        || progress.is_dead(2).unwrap_or(false),
        || Some(bank2_hash),
    );
    check_slot_agrees_with_cluster(
        2,
        bank_forks.read().unwrap().root(),
        &blockstore,
        &mut duplicate_slots_tracker,
        &mut epoch_slots_frozen_slots,
        &mut vote_simulator.tbft_structs.heaviest_subtree_fork_choice,
        &mut DuplicateSlotsToRepair::default(),
        &ancestor_hashes_replay_update_sender,
        &mut PurgeRepairSlotCounter::default(),
        SlotStateUpdate::Duplicate(duplicate_state),
    );

    let (vote_fork, reset_fork, _) = run_compute_and_select_forks(
        &bank_forks,
        &mut progress,
        &mut tower,
        &mut vote_simulator.tbft_structs.heaviest_subtree_fork_choice,
        &mut vote_simulator.latest_validator_votes_for_frozen_banks,
        None,
        &mut vote_slots,
    );

    // Should now pick the next heaviest fork that is not a descendant of 2, which is 6.
    // However the lockout from vote 4 should still apply, so 6 should not be votable
    assert!(vote_fork.is_none());
    assert_eq!(reset_fork.unwrap(), 6);

    // If slot 4 is marked as confirmed, then this confirms slot 2 and 4, and
    // then slot 4 is now the heaviest bank again
    let mut duplicate_slots_to_repair = DuplicateSlotsToRepair::default();
    duplicate_confirmed_slots.insert(4, bank4_hash);
    let duplicate_confirmed_state = DuplicateConfirmedState::new_from_state(
        bank4_hash,
        || progress.is_dead(4).unwrap_or(false),
        || Some(bank4_hash),
    );
    check_slot_agrees_with_cluster(
        4,
        bank_forks.read().unwrap().root(),
        &blockstore,
        &mut duplicate_slots_tracker,
        &mut epoch_slots_frozen_slots,
        &mut vote_simulator.tbft_structs.heaviest_subtree_fork_choice,
        &mut duplicate_slots_to_repair,
        &ancestor_hashes_replay_update_sender,
        &mut PurgeRepairSlotCounter::default(),
        SlotStateUpdate::DuplicateConfirmed(duplicate_confirmed_state),
    );
    // The confirmed hash is detected in `progress`, which means
    // it's confirmation on the replayed block. This means we have
    // the right version of the block, so `duplicate_slots_to_repair`
    // should be empty
    assert!(duplicate_slots_to_repair.is_empty());
    let (vote_fork, reset_fork, _) = run_compute_and_select_forks(
        &bank_forks,
        &mut progress,
        &mut tower,
        &mut vote_simulator.tbft_structs.heaviest_subtree_fork_choice,
        &mut vote_simulator.latest_validator_votes_for_frozen_banks,
        None,
        &mut vote_slots,
    );
    // Should now pick the heaviest fork 4 again, but lockouts apply so fork 4
    // is not votable, which avoids voting for 4 again.
    assert!(vote_fork.is_none());
    assert_eq!(reset_fork.unwrap(), 4);
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

fn setup_vote_then_rollback(
    first_vote: Slot,
    num_validators: usize,
    generate_votes: Option<GenerateVotes>,
) -> ReplayBlockstoreComponents {
    /*
        Build fork structure:

             slot 0
               |
             slot 1
             /    \
        slot 2    |
           |      |
        slot 3    |
           |      |
        slot 4    |
           |      |
        slot 5    |
                slot 6
                  |
                slot 7
    */
    let forks = tr(0) / (tr(1) / (tr(2) / (tr(3) / (tr(4) / (tr(5))))) / (tr(6) / (tr(7))));

    let mut replay_components =
        replay_blockstore_components(Some(forks), num_validators, generate_votes);

    let ReplayBlockstoreComponents {
        ref mut tower,
        ref blockstore,
        ref mut vote_simulator,
        ref leader_schedule_cache,
        ..
    } = replay_components;

    let &mut VoteSimulator {
        ref mut progress,
        ref bank_forks,
        ref mut tbft_structs,
        ..
    } = vote_simulator;

    tower.record_bank_vote(&bank_forks.read().unwrap().get(first_vote).unwrap());

    // Simulate another version of slot 2 was duplicate confirmed
    let our_bank2_hash = bank_forks.read().unwrap().bank_hash(2).unwrap();
    let duplicate_confirmed_bank2_hash = Hash::new_unique();
    let mut duplicate_confirmed_slots = DuplicateConfirmedSlots::default();
    duplicate_confirmed_slots.insert(2, duplicate_confirmed_bank2_hash);
    let mut duplicate_slots_tracker = DuplicateSlotsTracker::default();
    let mut duplicate_slots_to_repair = DuplicateSlotsToRepair::default();
    let mut epoch_slots_frozen_slots = EpochSlotsFrozenSlots::default();

    // Mark fork choice branch as invalid so select forks below doesn't panic
    // on a nonexistent `heaviest_bank_on_same_fork` after we dump the duplicate fork.
    let duplicate_confirmed_state = DuplicateConfirmedState::new_from_state(
        duplicate_confirmed_bank2_hash,
        || progress.is_dead(2).unwrap_or(false),
        || Some(our_bank2_hash),
    );
    let (ancestor_hashes_replay_update_sender, _ancestor_hashes_replay_update_receiver) =
        bounded(1024);
    check_slot_agrees_with_cluster(
        2,
        bank_forks.read().unwrap().root(),
        blockstore,
        &mut duplicate_slots_tracker,
        &mut epoch_slots_frozen_slots,
        &mut tbft_structs.heaviest_subtree_fork_choice,
        &mut duplicate_slots_to_repair,
        &ancestor_hashes_replay_update_sender,
        &mut PurgeRepairSlotCounter::default(),
        SlotStateUpdate::DuplicateConfirmed(duplicate_confirmed_state),
    );
    assert_eq!(
        *duplicate_slots_to_repair.get(&2).unwrap(),
        duplicate_confirmed_bank2_hash
    );
    let mut ancestors = bank_forks.read().unwrap().ancestors();
    let mut descendants = bank_forks.read().unwrap().descendants();
    let old_descendants_of_2 = descendants.get(&2).unwrap().clone();
    let (dumped_slots_sender, _dumped_slots_receiver) = bounded(1024);

    ReplayStage::dump_then_repair_correct_slots(
        &mut duplicate_slots_to_repair,
        &mut ancestors,
        &mut descendants,
        progress,
        bank_forks,
        blockstore,
        None,
        &mut PurgeRepairSlotCounter::default(),
        &dumped_slots_sender,
        &Pubkey::new_unique(),
        leader_schedule_cache,
    );

    // Check everything was purged properly
    for purged_slot in std::iter::once(&2).chain(old_descendants_of_2.iter()) {
        assert!(!ancestors.contains_key(purged_slot));
        assert!(!descendants.contains_key(purged_slot));
    }

    replay_components
}

fn run_test_duplicate_rollback_then_vote(first_vote: Slot) -> SelectVoteAndResetForkResult {
    let replay_components = setup_vote_then_rollback(
        first_vote,
        2,
        Some(Box::new(|node_keys| {
            // Simulate everyone else voting on 6, so we have enough to
            // make a switch to the other fork
            node_keys.into_iter().map(|k| (k, vec![6])).collect()
        })),
    );

    let ReplayBlockstoreComponents {
        mut tower,
        vote_simulator,
        ..
    } = replay_components;
    let mut vote_slots = HashSet::default();

    let VoteSimulator {
        mut progress,
        bank_forks,
        mut tbft_structs,
        mut latest_validator_votes_for_frozen_banks,
        ..
    } = vote_simulator;

    let mut frozen_banks: Vec<_> = bank_forks
        .read()
        .unwrap()
        .frozen_banks()
        .map(|(_slot, bank)| bank)
        .collect();

    let ancestors = bank_forks.read().unwrap().ancestors();
    let descendants = bank_forks.read().unwrap().descendants();

    ReplayStage::compute_bank_stats(
        &Pubkey::new_unique(),
        &ancestors,
        &mut frozen_banks,
        &mut tower,
        &mut progress,
        &VoteTracker::default(),
        &ClusterSlots::default_for_tests(),
        &bank_forks,
        &mut tbft_structs.heaviest_subtree_fork_choice,
        &mut latest_validator_votes_for_frozen_banks,
        &mut vote_slots,
        &MigrationStatus::default(),
    );

    // Try to switch to vote to the heaviest slot 6, then return the vote results
    let (heaviest_bank, heaviest_bank_on_same_fork) = tbft_structs
        .heaviest_subtree_fork_choice
        .select_forks(&frozen_banks, &tower, &progress, &ancestors, &bank_forks);
    assert_eq!(heaviest_bank.slot(), 7);
    assert!(heaviest_bank_on_same_fork.is_none());
    select_vote_and_reset_forks(
        &heaviest_bank,
        heaviest_bank_on_same_fork.as_ref(),
        &ancestors,
        &descendants,
        &progress,
        &mut tower,
        &latest_validator_votes_for_frozen_banks,
        &tbft_structs.heaviest_subtree_fork_choice,
    )
}

#[test]
fn test_duplicate_rollback_then_vote_locked_out() {
    let SelectVoteAndResetForkResult {
        vote_bank,
        reset_bank,
        heaviest_fork_failures,
    } = run_test_duplicate_rollback_then_vote(5);

    // If we vote on 5 first then try to vote on 7, we should be locked out,
    // despite the rollback
    assert!(vote_bank.is_none());
    assert_eq!(reset_bank.unwrap().slot(), 7);
    assert_eq!(
        heaviest_fork_failures,
        vec![HeaviestForkFailures::LockedOut(7)]
    );
}

#[test]
fn test_duplicate_rollback_then_vote_success() {
    let SelectVoteAndResetForkResult {
        vote_bank,
        reset_bank,
        heaviest_fork_failures,
    } = run_test_duplicate_rollback_then_vote(4);

    // If we vote on 4 first then try to vote on 7, we should succeed
    assert_matches!(
        vote_bank
            .map(|(bank, switch_decision)| (bank.slot(), switch_decision))
            .unwrap(),
        (7, SwitchForkDecision::SwitchProof(_))
    );
    assert_eq!(reset_bank.unwrap().slot(), 7);
    assert!(heaviest_fork_failures.is_empty());
}

fn run_test_duplicate_rollback_then_vote_on_other_duplicate(
    first_vote: Slot,
) -> SelectVoteAndResetForkResult {
    let replay_components = setup_vote_then_rollback(first_vote, 10, None::<GenerateVotes>);

    let ReplayBlockstoreComponents {
        mut tower,
        mut vote_simulator,
        ..
    } = replay_components;

    // Simulate repairing an alternate version of slot 2, 3 and 4 that we just dumped. Because
    // we're including votes this time for slot 1, it should generate a different
    // version of 2.
    let cluster_votes: HashMap<Pubkey, Vec<Slot>> = vote_simulator
        .node_pubkeys
        .iter()
        .map(|k| (*k, vec![1, 2]))
        .collect();

    // Create new versions of slots 2, 3, 4, 5, with parent slot 1
    vote_simulator.create_and_vote_new_branch(
        1,
        5,
        &cluster_votes,
        &HashSet::new(),
        &Pubkey::new_unique(),
        &mut tower,
    );

    let VoteSimulator {
        mut progress,
        bank_forks,
        mut tbft_structs,
        mut latest_validator_votes_for_frozen_banks,
        ..
    } = vote_simulator;

    // Check that the new branch with slot 2 is different than the original version.
    let bank_1_hash = bank_forks.read().unwrap().bank_hash(1).unwrap();
    let children_of_1 = (&tbft_structs.heaviest_subtree_fork_choice)
        .children(&(1, bank_1_hash))
        .unwrap();
    let duplicate_versions_of_2 = children_of_1.filter(|(slot, _hash)| *slot == 2).count();
    assert_eq!(duplicate_versions_of_2, 2);

    let mut frozen_banks: Vec<_> = bank_forks
        .read()
        .unwrap()
        .frozen_banks()
        .map(|(_slot, bank)| bank)
        .collect();

    let ancestors = bank_forks.read().unwrap().ancestors();
    let descendants = bank_forks.read().unwrap().descendants();
    let mut vote_slots = HashSet::default();

    ReplayStage::compute_bank_stats(
        &Pubkey::new_unique(),
        &ancestors,
        &mut frozen_banks,
        &mut tower,
        &mut progress,
        &VoteTracker::default(),
        &ClusterSlots::default_for_tests(),
        &bank_forks,
        &mut tbft_structs.heaviest_subtree_fork_choice,
        &mut latest_validator_votes_for_frozen_banks,
        &mut vote_slots,
        &MigrationStatus::default(),
    );
    // Try to switch to vote to the heaviest slot 5, then return the vote results
    let (heaviest_bank, heaviest_bank_on_same_fork) = tbft_structs
        .heaviest_subtree_fork_choice
        .select_forks(&frozen_banks, &tower, &progress, &ancestors, &bank_forks);
    assert_eq!(heaviest_bank.slot(), 5);
    assert!(heaviest_bank_on_same_fork.is_none());
    select_vote_and_reset_forks(
        &heaviest_bank,
        heaviest_bank_on_same_fork.as_ref(),
        &ancestors,
        &descendants,
        &progress,
        &mut tower,
        &latest_validator_votes_for_frozen_banks,
        &tbft_structs.heaviest_subtree_fork_choice,
    )
}

#[test]
fn test_duplicate_rollback_then_vote_on_other_duplicate_success() {
    let SelectVoteAndResetForkResult {
        vote_bank,
        reset_bank,
        heaviest_fork_failures,
    } = run_test_duplicate_rollback_then_vote_on_other_duplicate(3);

    // If we vote on 2 first then try to vote on 5, we should succeed
    assert_matches!(
        vote_bank
            .map(|(bank, switch_decision)| (bank.slot(), switch_decision))
            .unwrap(),
        (5, SwitchForkDecision::SwitchProof(_))
    );
    assert_eq!(reset_bank.unwrap().slot(), 5);
    assert!(heaviest_fork_failures.is_empty());
}

#[test]
fn test_duplicate_rollback_then_vote_on_other_duplicate_same_slot_locked_out() {
    let SelectVoteAndResetForkResult {
        vote_bank,
        reset_bank,
        heaviest_fork_failures,
    } = run_test_duplicate_rollback_then_vote_on_other_duplicate(5);

    // If we vote on 5 first then try to vote on another version of 5,
    // lockout should fail
    assert!(vote_bank.is_none());
    assert_eq!(reset_bank.unwrap().slot(), 5);
    assert_eq!(
        heaviest_fork_failures,
        vec![HeaviestForkFailures::LockedOut(5)]
    );
}

#[test]
#[ignore]
fn test_duplicate_rollback_then_vote_on_other_duplicate_different_slot_locked_out() {
    let SelectVoteAndResetForkResult {
        vote_bank,
        reset_bank,
        heaviest_fork_failures,
    } = run_test_duplicate_rollback_then_vote_on_other_duplicate(4);

    // If we vote on 4 first then try to vote on 5 descended from another version
    // of 4, lockout should fail
    assert!(vote_bank.is_none());
    assert_eq!(reset_bank.unwrap().slot(), 5);
    assert_eq!(
        heaviest_fork_failures,
        vec![HeaviestForkFailures::LockedOut(5)]
    );
}

#[test]
fn test_gossip_vote_doesnt_affect_fork_choice() {
    let (
        VoteSimulator {
            bank_forks,
            mut tbft_structs,
            mut latest_validator_votes_for_frozen_banks,
            vote_pubkeys,
            ..
        },
        _,
    ) = setup_default_forks(1, None::<GenerateVotes>);

    let vote_pubkey = vote_pubkeys[0];
    let mut unfrozen_gossip_verified_vote_hashes = UnfrozenGossipVerifiedVoteHashes::default();
    let (gossip_verified_vote_hash_sender, gossip_verified_vote_hash_receiver) = bounded(1024);

    // Best slot is 4
    assert_eq!(
        tbft_structs
            .heaviest_subtree_fork_choice
            .best_overall_slot()
            .0,
        4
    );

    // Cast a vote for slot 3 on one fork
    let vote_slot = 3;
    let vote_bank = bank_forks.read().unwrap().get(vote_slot).unwrap();
    gossip_verified_vote_hash_sender
        .send((vote_pubkey, vote_slot, vote_bank.hash()))
        .expect("Send should succeed");
    ReplayStage::process_gossip_verified_vote_hashes(
        &gossip_verified_vote_hash_receiver,
        &mut unfrozen_gossip_verified_vote_hashes,
        &tbft_structs.heaviest_subtree_fork_choice,
        &mut latest_validator_votes_for_frozen_banks,
    );

    // Pick the best fork. Gossip votes shouldn't affect fork choice
    tbft_structs
        .heaviest_subtree_fork_choice
        .compute_bank_stats(
            &vote_bank,
            &Tower::default(),
            &mut latest_validator_votes_for_frozen_banks,
        );

    // Best slot is still 4
    assert_eq!(
        tbft_structs
            .heaviest_subtree_fork_choice
            .best_overall_slot()
            .0,
        4
    );
}

#[test]
fn test_replay_stage_refresh_last_vote() {
    let ReplayBlockstoreComponents {
        cluster_info,
        poh_recorder,
        mut tower,
        my_pubkey,
        vote_simulator,
        ..
    } = replay_blockstore_components(None, 10, None::<GenerateVotes>);
    let tower_storage = NullTowerStorage::default();

    let VoteSimulator {
        mut validator_keypairs,
        bank_forks,
        mut progress,
        ..
    } = vote_simulator;

    let mut last_vote_refresh_time = LastVoteRefreshTime {
        last_refresh_time: Instant::now(),
        last_print_time: Instant::now(),
    };
    let has_new_vote_been_rooted = false;
    let mut tracked_vote_transactions = vec![];

    let identity_keypair = cluster_info.keypair();
    let my_vote_keypair = vec![Arc::new(
        validator_keypairs.remove(&my_pubkey).unwrap().vote_keypair,
    )];
    let my_vote_pubkey = my_vote_keypair[0].pubkey();
    let bank0 = bank_forks.read().unwrap().get(0).unwrap();
    progress.insert(
        bank0.slot(),
        ForkProgress::new_from_bank(
            &bank0,
            bank0.leader_id(),
            &Pubkey::default(),
            None,
            0,
            0,
            None,
        ),
    );

    let (voting_sender, voting_receiver) = bounded(1024);

    // Simulate landing a vote for slot 0 landing in slot 1
    let bank1 = Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        bank0.clone(),
        SlotLeader::default(),
        1,
    );
    bank1.fill_bank_with_ticks_for_tests();
    progress.insert(
        bank1.slot(),
        ForkProgress::new_from_bank(
            &bank1,
            bank1.leader_id(),
            &Pubkey::default(),
            None,
            0,
            0,
            None,
        ),
    );
    tower.record_bank_vote(&bank0);
    ReplayStage::push_vote(
        &bank0,
        &my_vote_pubkey,
        &identity_keypair,
        &my_vote_keypair,
        &mut tower,
        &SwitchForkDecision::SameFork,
        &mut tracked_vote_transactions,
        has_new_vote_been_rooted,
        &mut ReplayLoopTiming::default(),
        &voting_sender,
        None,
    );
    let vote_info = voting_receiver
        .recv_timeout(Duration::from_secs(1))
        .unwrap();

    let connection_cache = if DEFAULT_VOTE_USE_QUIC {
        ConnectionCache::new_quic_for_tests(
            "connection_cache_vote_quic",
            DEFAULT_TPU_CONNECTION_POOL_SIZE,
        )
    } else {
        ConnectionCache::with_udp(
            "connection_cache_vote_udp",
            DEFAULT_TPU_CONNECTION_POOL_SIZE,
        )
    };

    crate::voting_service::VotingService::handle_vote(
        &cluster_info,
        &poh_recorder,
        &tower_storage,
        vote_info,
        Arc::new(connection_cache),
    );

    let mut cursor = Cursor::default();
    let votes = cluster_info.get_votes(&mut cursor);
    assert_eq!(votes.len(), 1);
    let vote_tx = &votes[0];
    assert_eq!(vote_tx.message.recent_blockhash, bank0.last_blockhash());
    assert_eq!(
        tower.last_vote_tx_blockhash(),
        BlockhashStatus::Blockhash(bank0.last_blockhash())
    );
    assert_eq!(tower.last_voted_slot().unwrap(), 0);
    bank1.process_transaction(vote_tx).unwrap();
    bank1.freeze();

    // Trying to refresh the vote for bank 0 in bank 1 or bank 2 won't succeed because
    // the last vote has landed already
    let bank2 = Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        bank1.clone(),
        SlotLeader::default(),
        2,
    );
    bank2.fill_bank_with_ticks_for_tests();
    bank2.freeze();
    progress.insert(
        bank2.slot(),
        ForkProgress::new_from_bank(
            &bank2,
            bank2.leader_id(),
            &Pubkey::default(),
            None,
            0,
            0,
            None,
        ),
    );
    for refresh_bank in &[bank1.clone(), bank2.clone()] {
        progress
            .get_fork_stats_mut(refresh_bank.slot())
            .unwrap()
            .my_latest_landed_vote = Tower::last_voted_slot_in_bank(refresh_bank, &my_vote_pubkey);
        assert!(!ReplayStage::maybe_refresh_last_vote(
            &mut tower,
            &progress,
            Some(refresh_bank.clone()),
            &my_vote_pubkey,
            &identity_keypair,
            &my_vote_keypair,
            &mut tracked_vote_transactions,
            has_new_vote_been_rooted,
            &mut last_vote_refresh_time,
            &voting_sender,
            None,
        ));

        // No new votes have been submitted to gossip
        let votes = cluster_info.get_votes(&mut cursor);
        assert!(votes.is_empty());
        // Tower's latest vote tx blockhash hasn't changed either
        assert_eq!(
            tower.last_vote_tx_blockhash(),
            BlockhashStatus::Blockhash(bank0.last_blockhash())
        );
        assert_eq!(tower.last_voted_slot().unwrap(), 0);
    }

    // Simulate submitting a new vote for bank 1 to the network, but the vote
    // not landing
    tower.record_bank_vote(&bank1);
    ReplayStage::push_vote(
        &bank1,
        &my_vote_pubkey,
        &identity_keypair,
        &my_vote_keypair,
        &mut tower,
        &SwitchForkDecision::SameFork,
        &mut tracked_vote_transactions,
        has_new_vote_been_rooted,
        &mut ReplayLoopTiming::default(),
        &voting_sender,
        None,
    );
    let vote_info = voting_receiver
        .recv_timeout(Duration::from_secs(1))
        .unwrap();

    let connection_cache = if DEFAULT_VOTE_USE_QUIC {
        ConnectionCache::new_quic_for_tests(
            "connection_cache_vote_quic",
            DEFAULT_TPU_CONNECTION_POOL_SIZE,
        )
    } else {
        ConnectionCache::with_udp(
            "connection_cache_vote_udp",
            DEFAULT_TPU_CONNECTION_POOL_SIZE,
        )
    };

    crate::voting_service::VotingService::handle_vote(
        &cluster_info,
        &poh_recorder,
        &tower_storage,
        vote_info,
        Arc::new(connection_cache),
    );

    let votes = cluster_info.get_votes(&mut cursor);
    assert_eq!(votes.len(), 1);
    let vote_tx = &votes[0];
    assert_eq!(vote_tx.message.recent_blockhash, bank1.last_blockhash());
    assert_eq!(
        tower.last_vote_tx_blockhash(),
        BlockhashStatus::Blockhash(bank1.last_blockhash())
    );
    assert_eq!(tower.last_voted_slot().unwrap(), 1);

    // Trying to refresh the vote for bank 1 in bank 2 won't succeed because
    // the blockheight is not increased enough
    progress
        .get_fork_stats_mut(bank1.slot())
        .unwrap()
        .block_height = bank1.block_height();
    progress
        .get_fork_stats_mut(bank2.slot())
        .unwrap()
        .block_height = bank2.block_height();
    progress
        .get_fork_stats_mut(bank2.slot())
        .unwrap()
        .my_latest_landed_vote = Tower::last_voted_slot_in_bank(&bank2, &my_vote_pubkey);
    assert!(!ReplayStage::maybe_refresh_last_vote(
        &mut tower,
        &progress,
        Some(bank2.clone()),
        &my_vote_pubkey,
        &identity_keypair,
        &my_vote_keypair,
        &mut tracked_vote_transactions,
        has_new_vote_been_rooted,
        &mut last_vote_refresh_time,
        &voting_sender,
        None,
    ));

    // No new votes have been submitted to gossip
    let votes = cluster_info.get_votes(&mut cursor);
    assert!(votes.is_empty());
    assert_eq!(
        tower.last_vote_tx_blockhash(),
        BlockhashStatus::Blockhash(bank1.last_blockhash())
    );
    assert_eq!(tower.last_voted_slot().unwrap(), 1);

    // Create a bank where the last vote transaction will have expired
    let expired_bank = {
        let mut parent_bank = bank2.clone();
        for _ in 0..REFRESH_VOTE_BLOCKHEIGHT {
            let slot = parent_bank.slot() + 1;
            parent_bank = Bank::new_from_parent_with_bank_forks(
                bank_forks.as_ref(),
                parent_bank,
                SlotLeader::default(),
                slot,
            );
            parent_bank.fill_bank_with_ticks_for_tests();
            parent_bank.freeze();
        }
        parent_bank
    };
    progress.insert(
        expired_bank.slot(),
        ForkProgress::new_from_bank(
            &expired_bank,
            expired_bank.leader_id(),
            &Pubkey::default(),
            None,
            0,
            0,
            None,
        ),
    );

    // Now trying to refresh the vote for slot 1 will succeed because the blockheight has increased
    // enough
    progress
        .get_fork_stats_mut(expired_bank.slot())
        .unwrap()
        .my_latest_landed_vote = Tower::last_voted_slot_in_bank(&expired_bank, &my_vote_pubkey);
    progress
        .get_fork_stats_mut(expired_bank.slot())
        .unwrap()
        .block_height = expired_bank.block_height();
    last_vote_refresh_time.last_refresh_time = last_vote_refresh_time
        .last_refresh_time
        .checked_sub(Duration::from_millis(
            MAX_VOTE_REFRESH_INTERVAL_MILLIS as u64 + 1,
        ))
        .unwrap();
    let clone_refresh_time = last_vote_refresh_time.last_refresh_time;
    assert!(ReplayStage::maybe_refresh_last_vote(
        &mut tower,
        &progress,
        Some(expired_bank.clone()),
        &my_vote_pubkey,
        &identity_keypair,
        &my_vote_keypair,
        &mut tracked_vote_transactions,
        has_new_vote_been_rooted,
        &mut last_vote_refresh_time,
        &voting_sender,
        None,
    ));
    let vote_info = voting_receiver
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let connection_cache = if DEFAULT_VOTE_USE_QUIC {
        ConnectionCache::new_quic_for_tests(
            "connection_cache_vote_quic",
            DEFAULT_TPU_CONNECTION_POOL_SIZE,
        )
    } else {
        ConnectionCache::with_udp(
            "connection_cache_vote_udp",
            DEFAULT_TPU_CONNECTION_POOL_SIZE,
        )
    };

    crate::voting_service::VotingService::handle_vote(
        &cluster_info,
        &poh_recorder,
        &tower_storage,
        vote_info,
        Arc::new(connection_cache),
    );

    assert!(last_vote_refresh_time.last_refresh_time > clone_refresh_time);
    let votes = cluster_info.get_votes(&mut cursor);
    assert_eq!(votes.len(), 1);
    let vote_tx = &votes[0];
    assert_eq!(
        vote_tx.message.recent_blockhash,
        expired_bank.last_blockhash()
    );
    assert_eq!(
        tower.last_vote_tx_blockhash(),
        BlockhashStatus::Blockhash(expired_bank.last_blockhash())
    );
    assert_eq!(tower.last_voted_slot().unwrap(), 1);

    // Processing the vote transaction should be valid
    let expired_bank_child_slot = expired_bank.slot() + 1;
    let expired_bank_child = Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        expired_bank.clone(),
        SlotLeader::default(),
        expired_bank_child_slot,
    );
    expired_bank_child.process_transaction(vote_tx).unwrap();
    let vote_account = expired_bank_child
        .get_vote_account(&my_vote_pubkey)
        .unwrap();
    assert_eq!(
        vote_account
            .vote_state_view()
            .votes_iter()
            .map(|lockout| lockout.slot())
            .collect_vec(),
        vec![0, 1]
    );
    expired_bank_child.fill_bank_with_ticks_for_tests();
    expired_bank_child.freeze();

    // Trying to refresh the vote on a sibling bank where:
    // 1) The vote for slot 1 hasn't landed
    // 2) The blockheight is still eligible for a refresh
    // This will still not refresh because `MAX_VOTE_REFRESH_INTERVAL_MILLIS` has not expired yet
    let expired_bank_sibling = {
        let mut parent_bank = bank2;
        for i in 0..expired_bank_child_slot {
            let slot = expired_bank_child.slot() + i + 1;
            parent_bank = Bank::new_from_parent_with_bank_forks(
                bank_forks.as_ref(),
                parent_bank,
                SlotLeader::default(),
                slot,
            );
            parent_bank.fill_bank_with_ticks_for_tests();
            parent_bank.freeze();
        }
        parent_bank
    };
    // Set the last refresh to now, shouldn't refresh because the last refresh just happened.
    last_vote_refresh_time.last_refresh_time = Instant::now();
    ReplayStage::maybe_refresh_last_vote(
        &mut tower,
        &progress,
        Some(expired_bank_sibling),
        &my_vote_pubkey,
        &identity_keypair,
        &my_vote_keypair,
        &mut tracked_vote_transactions,
        has_new_vote_been_rooted,
        &mut last_vote_refresh_time,
        &voting_sender,
        None,
    );

    let votes = cluster_info.get_votes(&mut cursor);
    assert!(votes.is_empty());
    assert_eq!(
        vote_tx.message.recent_blockhash,
        expired_bank.last_blockhash()
    );
    assert_eq!(
        tower.last_vote_tx_blockhash(),
        BlockhashStatus::Blockhash(expired_bank.last_blockhash())
    );
    assert_eq!(tower.last_voted_slot().unwrap(), 1);
}

#[allow(clippy::too_many_arguments)]
fn send_vote_in_new_bank(
    parent_bank: Arc<Bank>,
    my_slot: Slot,
    my_vote_keypair: &[Arc<Keypair>],
    tower: &mut Tower,
    identity_keypair: &Keypair,
    tracked_vote_transactions: &mut Vec<TrackedVoteTransaction>,
    has_new_vote_been_rooted: bool,
    voting_sender: &Sender<VoteOp>,
    voting_receiver: &Receiver<VoteOp>,
    cluster_info: &ClusterInfo,
    poh_recorder: &RwLock<PohRecorder>,
    tower_storage: &dyn TowerStorage,
    make_it_landing: bool,
    cursor: &mut Cursor,
    bank_forks: Arc<RwLock<BankForks>>,
    progress: &mut ProgressMap,
) -> Arc<Bank> {
    let my_vote_pubkey = &my_vote_keypair[0].pubkey();
    tower.record_bank_vote(&parent_bank);
    ReplayStage::push_vote(
        &parent_bank,
        my_vote_pubkey,
        identity_keypair,
        my_vote_keypair,
        tower,
        &SwitchForkDecision::SameFork,
        tracked_vote_transactions,
        has_new_vote_been_rooted,
        &mut ReplayLoopTiming::default(),
        voting_sender,
        None,
    );
    let vote_info = voting_receiver
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let connection_cache = if DEFAULT_VOTE_USE_QUIC {
        ConnectionCache::new_quic_for_tests(
            "connection_cache_vote_quic",
            DEFAULT_TPU_CONNECTION_POOL_SIZE,
        )
    } else {
        ConnectionCache::with_udp(
            "connection_cache_vote_udp",
            DEFAULT_TPU_CONNECTION_POOL_SIZE,
        )
    };

    crate::voting_service::VotingService::handle_vote(
        cluster_info,
        poh_recorder,
        tower_storage,
        vote_info,
        Arc::new(connection_cache),
    );

    let votes = cluster_info.get_votes(cursor);
    assert_eq!(votes.len(), 1);
    let vote_tx = &votes[0];
    assert_eq!(
        vote_tx.message.recent_blockhash,
        parent_bank.last_blockhash()
    );
    assert_eq!(
        tower.last_vote_tx_blockhash(),
        BlockhashStatus::Blockhash(parent_bank.last_blockhash())
    );
    assert_eq!(tower.last_voted_slot().unwrap(), parent_bank.slot());
    let bank = Bank::new_from_parent_with_bank_forks(
        &bank_forks,
        parent_bank,
        SlotLeader::default(),
        my_slot,
    );
    bank.fill_bank_with_ticks_for_tests();
    if make_it_landing {
        bank.process_transaction(vote_tx).unwrap();
    }
    bank.freeze();
    progress.entry(my_slot).or_insert_with(|| {
        ForkProgress::new_from_bank(
            &bank,
            &identity_keypair.pubkey(),
            my_vote_pubkey,
            None,
            0,
            0,
            None,
        )
    });
    bank_forks.read().unwrap().get(my_slot).unwrap()
}

#[test]
fn test_replay_stage_last_vote_outside_slot_hashes() {
    agave_logger::setup();
    let ReplayBlockstoreComponents {
        cluster_info,
        poh_recorder,
        mut tower,
        my_pubkey,
        vote_simulator,
        ..
    } = replay_blockstore_components(None, 10, None::<GenerateVotes>);
    let tower_storage = NullTowerStorage::default();

    let VoteSimulator {
        mut validator_keypairs,
        bank_forks,
        mut tbft_structs,
        mut latest_validator_votes_for_frozen_banks,
        mut progress,
        ..
    } = vote_simulator;
    let mut vote_slots = HashSet::default();

    let has_new_vote_been_rooted = false;
    let mut tracked_vote_transactions = vec![];

    let identity_keypair = cluster_info.keypair();
    let my_vote_keypair = vec![Arc::new(
        validator_keypairs.remove(&my_pubkey).unwrap().vote_keypair,
    )];
    let my_vote_pubkey = my_vote_keypair[0].pubkey();
    let bank0 = bank_forks.read().unwrap().get(0).unwrap();

    // Add a new fork starting from 0 with bigger slot number, we assume it has a bigger
    // weight, but we cannot switch because of lockout.
    let other_fork_slot = 1;
    let other_fork_bank = Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        bank0.clone(),
        SlotLeader::default(),
        other_fork_slot,
    );
    other_fork_bank.fill_bank_with_ticks_for_tests();
    other_fork_bank.freeze();
    progress.entry(other_fork_slot).or_insert_with(|| {
        ForkProgress::new_from_bank(
            &other_fork_bank,
            &identity_keypair.pubkey(),
            &my_vote_keypair[0].pubkey(),
            None,
            0,
            0,
            None,
        )
    });

    let (voting_sender, voting_receiver) = bounded(1024);
    let mut cursor = Cursor::default();

    let mut new_bank = send_vote_in_new_bank(
        bank0,
        2,
        &my_vote_keypair,
        &mut tower,
        &identity_keypair,
        &mut tracked_vote_transactions,
        has_new_vote_been_rooted,
        &voting_sender,
        &voting_receiver,
        &cluster_info,
        &poh_recorder,
        &tower_storage,
        true,
        &mut cursor,
        bank_forks.clone(),
        &mut progress,
    );
    new_bank = send_vote_in_new_bank(
        new_bank.clone(),
        new_bank.slot() + 1,
        &my_vote_keypair,
        &mut tower,
        &identity_keypair,
        &mut tracked_vote_transactions,
        has_new_vote_been_rooted,
        &voting_sender,
        &voting_receiver,
        &cluster_info,
        &poh_recorder,
        &tower_storage,
        false,
        &mut cursor,
        bank_forks.clone(),
        &mut progress,
    );
    // Create enough banks on the fork so last vote is outside SlotHash, make sure
    // we now vote at the tip of the fork.
    let last_voted_slot = tower.last_voted_slot().unwrap();
    while new_bank.is_in_slot_hashes_history(&last_voted_slot) {
        let new_slot = new_bank.slot() + 1;
        let bank = Bank::new_from_parent_with_bank_forks(
            bank_forks.as_ref(),
            new_bank,
            SlotLeader::default(),
            new_slot,
        );
        bank.fill_bank_with_ticks_for_tests();
        bank.freeze();
        progress.entry(new_slot).or_insert_with(|| {
            ForkProgress::new_from_bank(
                &bank,
                &identity_keypair.pubkey(),
                &my_vote_keypair[0].pubkey(),
                None,
                0,
                0,
                None,
            )
        });
        new_bank = bank_forks.read().unwrap().get(new_slot).unwrap();
    }
    let tip_of_voted_fork = new_bank.slot();

    let mut frozen_banks: Vec<_> = bank_forks
        .read()
        .unwrap()
        .frozen_banks()
        .map(|(_slot, bank)| bank)
        .collect();
    ReplayStage::compute_bank_stats(
        &my_vote_pubkey,
        &bank_forks.read().unwrap().ancestors(),
        &mut frozen_banks,
        &mut tower,
        &mut progress,
        &VoteTracker::default(),
        &ClusterSlots::default_for_tests(),
        &bank_forks,
        &mut tbft_structs.heaviest_subtree_fork_choice,
        &mut latest_validator_votes_for_frozen_banks,
        &mut vote_slots,
        &MigrationStatus::default(),
    );
    assert_eq!(tower.last_voted_slot(), Some(last_voted_slot));
    assert_eq!(progress.my_latest_landed_vote(tip_of_voted_fork), Some(0));
    let other_fork_bank = &bank_forks.read().unwrap().get(other_fork_slot).unwrap();
    let SelectVoteAndResetForkResult { vote_bank, .. } = select_vote_and_reset_forks(
        other_fork_bank,
        Some(&new_bank),
        &bank_forks.read().unwrap().ancestors(),
        &bank_forks.read().unwrap().descendants(),
        &progress,
        &mut tower,
        &latest_validator_votes_for_frozen_banks,
        &tbft_structs.heaviest_subtree_fork_choice,
    );
    assert!(vote_bank.is_some());
    assert_eq!(vote_bank.unwrap().0.slot(), tip_of_voted_fork);

    // If last vote is already equal to heaviest_bank_on_same_voted_fork,
    // we should not vote.
    let last_voted_bank = &bank_forks.read().unwrap().get(last_voted_slot).unwrap();
    let SelectVoteAndResetForkResult { vote_bank, .. } = select_vote_and_reset_forks(
        other_fork_bank,
        Some(last_voted_bank),
        &bank_forks.read().unwrap().ancestors(),
        &bank_forks.read().unwrap().descendants(),
        &progress,
        &mut tower,
        &latest_validator_votes_for_frozen_banks,
        &tbft_structs.heaviest_subtree_fork_choice,
    );
    assert!(vote_bank.is_none());

    // If last vote is still inside slot hashes history of heaviest_bank_on_same_voted_fork,
    // we should not vote.
    let last_voted_bank_plus_1 = &bank_forks.read().unwrap().get(last_voted_slot + 1).unwrap();
    let SelectVoteAndResetForkResult { vote_bank, .. } = select_vote_and_reset_forks(
        other_fork_bank,
        Some(last_voted_bank_plus_1),
        &bank_forks.read().unwrap().ancestors(),
        &bank_forks.read().unwrap().descendants(),
        &progress,
        &mut tower,
        &latest_validator_votes_for_frozen_banks,
        &tbft_structs.heaviest_subtree_fork_choice,
    );
    assert!(vote_bank.is_none());

    // create a new bank and make last_voted_slot land, we should not vote.
    progress
        .entry(new_bank.slot())
        .and_modify(|s| s.fork_stats.my_latest_landed_vote = Some(last_voted_slot));
    assert!(!new_bank.is_in_slot_hashes_history(&last_voted_slot));
    let SelectVoteAndResetForkResult { vote_bank, .. } = select_vote_and_reset_forks(
        other_fork_bank,
        Some(&new_bank),
        &bank_forks.read().unwrap().ancestors(),
        &bank_forks.read().unwrap().descendants(),
        &progress,
        &mut tower,
        &latest_validator_votes_for_frozen_banks,
        &tbft_structs.heaviest_subtree_fork_choice,
    );
    assert!(vote_bank.is_none());
}

#[test]
fn test_retransmit_latest_unpropagated_leader_slot() {
    let ReplayBlockstoreComponents {
        validator_node_to_vote_keys,
        leader_schedule_cache,
        poh_recorder,
        vote_simulator,
        ..
    } = replay_blockstore_components(None, 10, None::<GenerateVotes>);

    let VoteSimulator {
        mut progress,
        ref bank_forks,
        ..
    } = vote_simulator;

    let poh_recorder = Arc::new(poh_recorder);
    let (retransmit_slots_sender, retransmit_slots_receiver) = bounded(1024);

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

    progress.get_retransmit_info_mut(0).unwrap().retry_time = Instant::now();
    ReplayStage::retransmit_latest_unpropagated_leader_slot(
        &poh_recorder,
        &retransmit_slots_sender,
        &mut progress,
    );
    let res = retransmit_slots_receiver.recv_timeout(Duration::from_millis(10));
    assert_matches!(res, Err(_));
    assert_eq!(
        progress.get_retransmit_info(0).unwrap().retry_iteration,
        0,
        "retransmit should not advance retry_iteration before time has been set"
    );

    ReplayStage::retransmit_latest_unpropagated_leader_slot(
        &poh_recorder,
        &retransmit_slots_sender,
        &mut progress,
    );
    let res = retransmit_slots_receiver.recv_timeout(Duration::from_millis(10));
    assert!(
        res.is_err(),
        "retry_iteration=0, elapsed < 2^0 * RETRANSMIT_BASE_DELAY_MS"
    );

    progress.get_retransmit_info_mut(0).unwrap().retry_time = Instant::now()
        .checked_sub(Duration::from_millis(RETRANSMIT_BASE_DELAY_MS + 1))
        .unwrap();
    ReplayStage::retransmit_latest_unpropagated_leader_slot(
        &poh_recorder,
        &retransmit_slots_sender,
        &mut progress,
    );
    let res = retransmit_slots_receiver.recv_timeout(Duration::from_millis(10));
    assert!(
        res.is_ok(),
        "retry_iteration=0, elapsed > RETRANSMIT_BASE_DELAY_MS"
    );
    assert_eq!(
        progress.get_retransmit_info(0).unwrap().retry_iteration,
        1,
        "retransmit should advance retry_iteration"
    );

    ReplayStage::retransmit_latest_unpropagated_leader_slot(
        &poh_recorder,
        &retransmit_slots_sender,
        &mut progress,
    );
    let res = retransmit_slots_receiver.recv_timeout(Duration::from_millis(10));
    assert!(
        res.is_err(),
        "retry_iteration=1, elapsed < 2^1 * RETRANSMIT_BASE_DELAY_MS"
    );

    progress.get_retransmit_info_mut(0).unwrap().retry_time = Instant::now()
        .checked_sub(Duration::from_millis(RETRANSMIT_BASE_DELAY_MS + 1))
        .unwrap();
    ReplayStage::retransmit_latest_unpropagated_leader_slot(
        &poh_recorder,
        &retransmit_slots_sender,
        &mut progress,
    );
    let res = retransmit_slots_receiver.recv_timeout(Duration::from_millis(10));
    assert!(
        res.is_err(),
        "retry_iteration=1, elapsed < 2^1 * RETRANSMIT_BASE_DELAY_MS"
    );

    progress.get_retransmit_info_mut(0).unwrap().retry_time = Instant::now()
        .checked_sub(Duration::from_millis(2 * RETRANSMIT_BASE_DELAY_MS + 1))
        .unwrap();
    ReplayStage::retransmit_latest_unpropagated_leader_slot(
        &poh_recorder,
        &retransmit_slots_sender,
        &mut progress,
    );
    let res = retransmit_slots_receiver.recv_timeout(Duration::from_millis(10));
    assert!(
        res.is_ok(),
        "retry_iteration=1, elapsed > 2^1 * RETRANSMIT_BASE_DELAY_MS"
    );
    assert_eq!(
        progress.get_retransmit_info(0).unwrap().retry_iteration,
        2,
        "retransmit should advance retry_iteration"
    );

    // increment to retry iteration 3
    progress
        .get_retransmit_info_mut(0)
        .unwrap()
        .increment_retry_iteration();

    progress.get_retransmit_info_mut(0).unwrap().retry_time = Instant::now()
        .checked_sub(Duration::from_millis(2 * RETRANSMIT_BASE_DELAY_MS + 1))
        .unwrap();
    ReplayStage::retransmit_latest_unpropagated_leader_slot(
        &poh_recorder,
        &retransmit_slots_sender,
        &mut progress,
    );
    let res = retransmit_slots_receiver.recv_timeout(Duration::from_millis(10));
    assert!(
        res.is_err(),
        "retry_iteration=3, elapsed < 2^3 * RETRANSMIT_BASE_DELAY_MS"
    );

    progress.get_retransmit_info_mut(0).unwrap().retry_time = Instant::now()
        .checked_sub(Duration::from_millis(8 * RETRANSMIT_BASE_DELAY_MS + 1))
        .unwrap();
    ReplayStage::retransmit_latest_unpropagated_leader_slot(
        &poh_recorder,
        &retransmit_slots_sender,
        &mut progress,
    );
    let res = retransmit_slots_receiver.recv_timeout(Duration::from_millis(10));
    assert!(
        res.is_ok(),
        "retry_iteration=3, elapsed > 2^3 * RETRANSMIT_BASE_DELAY"
    );
    assert_eq!(
        progress.get_retransmit_info(0).unwrap().retry_iteration,
        4,
        "retransmit should advance retry_iteration"
    );
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

fn run_compute_and_select_forks(
    bank_forks: &RwLock<BankForks>,
    progress: &mut ProgressMap,
    tower: &mut Tower,
    heaviest_subtree_fork_choice: &mut HeaviestSubtreeForkChoice,
    latest_validator_votes_for_frozen_banks: &mut LatestValidatorVotesForFrozenBanks,
    my_vote_pubkey: Option<Pubkey>,
    vote_slots: &mut HashSet<Slot, ahash::RandomState>,
) -> (Option<Slot>, Option<Slot>, Vec<HeaviestForkFailures>) {
    let mut frozen_banks: Vec<_> = bank_forks
        .read()
        .unwrap()
        .frozen_banks()
        .map(|(_slot, bank)| bank)
        .collect();
    let ancestors = &bank_forks.read().unwrap().ancestors();
    let descendants = &bank_forks.read().unwrap().descendants();
    ReplayStage::compute_bank_stats(
        &my_vote_pubkey.unwrap_or_default(),
        &bank_forks.read().unwrap().ancestors(),
        &mut frozen_banks,
        tower,
        progress,
        &VoteTracker::default(),
        &ClusterSlots::default_for_tests(),
        bank_forks,
        heaviest_subtree_fork_choice,
        latest_validator_votes_for_frozen_banks,
        vote_slots,
        &MigrationStatus::default(),
    );
    let (heaviest_bank, heaviest_bank_on_same_fork) = heaviest_subtree_fork_choice.select_forks(
        &frozen_banks,
        tower,
        progress,
        ancestors,
        bank_forks,
    );
    let SelectVoteAndResetForkResult {
        vote_bank,
        reset_bank,
        heaviest_fork_failures,
    } = select_vote_and_reset_forks(
        &heaviest_bank,
        heaviest_bank_on_same_fork.as_ref(),
        ancestors,
        descendants,
        progress,
        tower,
        latest_validator_votes_for_frozen_banks,
        heaviest_subtree_fork_choice,
    );
    (
        vote_bank.map(|(b, _)| b.slot()),
        reset_bank.map(|b| b.slot()),
        heaviest_fork_failures,
    )
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
fn test_tower_sync_from_bank_failed_switch() {
    agave_logger::setup_with_default(
        "error,solana_core::replay_stage=info,solana_core::consensus=info",
    );
    /*
        Fork structure:

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

        We had some point voted 0 - 6, while the rest of the network voted 0 - 4.
        We are sitting with an oudated tower that has voted until 1. We see that 4 is the heaviest slot,
        however in the past we have voted up to 6. We must acknowledge the vote state present at 6,
        adopt it as our own and *not* vote on 2 or 4, to respect slashing rules as there is
        not enough stake to switch
    */

    let generate_votes = |pubkeys: Vec<Pubkey>| {
        pubkeys
            .into_iter()
            .zip(iter::once(vec![0, 1, 3, 5, 6]).chain(iter::repeat_n(vec![0, 1, 2, 4], 2)))
            .collect()
    };
    let (mut vote_simulator, _blockstore) = setup_default_forks(3, Some(Box::new(generate_votes)));
    let (bank_forks, mut progress) = (vote_simulator.bank_forks, vote_simulator.progress);
    let bank_hash = |slot| bank_forks.read().unwrap().bank_hash(slot).unwrap();
    let my_vote_pubkey = vote_simulator.vote_pubkeys[0];
    let mut vote_slots = HashSet::default();
    let mut tower = Tower::default();
    tower.node_pubkey = vote_simulator.node_pubkeys[0];
    tower.record_vote(0, bank_hash(0));
    tower.record_vote(1, bank_hash(1));

    let (vote_fork, reset_fork, failures) = run_compute_and_select_forks(
        &bank_forks,
        &mut progress,
        &mut tower,
        &mut vote_simulator.tbft_structs.heaviest_subtree_fork_choice,
        &mut vote_simulator.latest_validator_votes_for_frozen_banks,
        Some(my_vote_pubkey),
        &mut vote_slots,
    );

    assert_eq!(vote_fork, None);
    assert_eq!(reset_fork, Some(6));
    assert_eq!(
        failures,
        vec![
            HeaviestForkFailures::FailedSwitchThreshold(4, 0, 30000),
            HeaviestForkFailures::LockedOut(4)
        ]
    );

    let (vote_fork, reset_fork, failures) = run_compute_and_select_forks(
        &bank_forks,
        &mut progress,
        &mut tower,
        &mut vote_simulator.tbft_structs.heaviest_subtree_fork_choice,
        &mut vote_simulator.latest_validator_votes_for_frozen_banks,
        Some(my_vote_pubkey),
        &mut vote_slots,
    );

    assert_eq!(vote_fork, None);
    assert_eq!(reset_fork, Some(6));
    assert_eq!(
        failures,
        vec![
            HeaviestForkFailures::FailedSwitchThreshold(4, 0, 30000),
            HeaviestForkFailures::LockedOut(4)
        ]
    );
}

#[test]
fn test_tower_sync_from_bank_failed_lockout() {
    agave_logger::setup_with_default(
        "error,solana_core::replay_stage=info,solana_core::consensus=info",
    );
    /*
        Fork structure:

             slot 0
               |
             slot 1
             /    \
        slot 3    |
           |    slot 2
        slot 4    |
                slot 5
                  |
                slot 6

        We had some point voted 0 - 6, while the rest of the network voted 0 - 4.
        We are sitting with an oudated tower that has voted until 1. We see that 4 is the heaviest slot,
        however in the past we have voted up to 6. We must acknowledge the vote state present at 6,
        adopt it as our own and *not* vote on 3 or 4, to respect slashing rules as we are locked
        out on 4, even though there is enough stake to switch. However we should still reset onto
        4.
    */

    let generate_votes = |pubkeys: Vec<Pubkey>| {
        pubkeys
            .into_iter()
            .zip(iter::once(vec![0, 1, 2, 5, 6]).chain(iter::repeat_n(vec![0, 1, 3, 4], 2)))
            .collect()
    };
    let tree = tr(0) / (tr(1) / (tr(3) / (tr(4))) / (tr(2) / (tr(5) / (tr(6)))));
    let (mut vote_simulator, _blockstore) =
        setup_forks_from_tree(tree, 3, Some(Box::new(generate_votes)));
    let (bank_forks, mut progress) = (vote_simulator.bank_forks, vote_simulator.progress);
    let bank_hash = |slot| bank_forks.read().unwrap().bank_hash(slot).unwrap();
    let my_vote_pubkey = vote_simulator.vote_pubkeys[0];
    let mut tower = Tower::default();
    tower.node_pubkey = vote_simulator.node_pubkeys[0];
    tower.record_vote(0, bank_hash(0));
    tower.record_vote(1, bank_hash(1));
    let mut vote_slots = HashSet::default();

    let (vote_fork, reset_fork, failures) = run_compute_and_select_forks(
        &bank_forks,
        &mut progress,
        &mut tower,
        &mut vote_simulator.tbft_structs.heaviest_subtree_fork_choice,
        &mut vote_simulator.latest_validator_votes_for_frozen_banks,
        Some(my_vote_pubkey),
        &mut vote_slots,
    );

    assert_eq!(vote_fork, None);
    assert_eq!(reset_fork, Some(4));
    assert_eq!(failures, vec![HeaviestForkFailures::LockedOut(4),]);

    let (vote_fork, reset_fork, failures) = run_compute_and_select_forks(
        &bank_forks,
        &mut progress,
        &mut tower,
        &mut vote_simulator.tbft_structs.heaviest_subtree_fork_choice,
        &mut vote_simulator.latest_validator_votes_for_frozen_banks,
        Some(my_vote_pubkey),
        &mut vote_slots,
    );

    assert_eq!(vote_fork, None);
    assert_eq!(reset_fork, Some(4));
    assert_eq!(failures, vec![HeaviestForkFailures::LockedOut(4),]);
}

#[test]
fn test_tower_adopt_from_bank_cache_only_computed() {
    agave_logger::setup_with_default(
        "error,solana_core::replay_stage=info,solana_core::consensus=info",
    );
    /*
        Fork structure:

             slot 0
               |
             slot 1
             /    \
        slot 3    |
           |    slot 2
        slot 4    |
                slot 5
                  |
                slot 6

        We had some point voted 0 - 6, we are sitting with an oudated tower that has voted until 1.
    */

    let generate_votes = |pubkeys: Vec<Pubkey>| {
        pubkeys
            .into_iter()
            .zip(iter::once(vec![0, 1, 2, 5, 6]).chain(iter::repeat_n(vec![0, 1, 3, 4], 2)))
            .collect()
    };
    let tree = tr(0) / (tr(1) / (tr(3) / (tr(4))) / (tr(2) / (tr(5) / (tr(6)))));
    let (vote_simulator, _blockstore) =
        setup_forks_from_tree(tree, 3, Some(Box::new(generate_votes)));
    let (bank_forks, mut progress) = (vote_simulator.bank_forks, vote_simulator.progress);
    let bank_hash = |slot| bank_forks.read().unwrap().bank_hash(slot).unwrap();
    let my_vote_pubkey = vote_simulator.vote_pubkeys[0];
    let mut tower = Tower::default();
    tower.node_pubkey = vote_simulator.node_pubkeys[0];
    tower.record_vote(0, bank_hash(0));
    tower.record_vote(1, bank_hash(1));

    let mut frozen_banks: Vec<_> = bank_forks
        .read()
        .unwrap()
        .frozen_banks()
        .map(|(_slot, bank)| bank)
        .collect();
    let ancestors = &bank_forks.read().unwrap().ancestors();

    // slot 3 was computed in a previous iteration and failed threshold check, but was not locked out
    let fork_stats_3 = progress.get_fork_stats_mut(3).unwrap();
    fork_stats_3.vote_threshold = vec![ThresholdDecision::FailedThreshold(4, 4000)];
    fork_stats_3.is_locked_out = false;
    fork_stats_3.computed = true;
    // slot 4 is yet to be computed.
    let fork_stats_4 = progress.get_fork_stats_mut(4).unwrap();
    fork_stats_4.computed = false;

    frozen_banks.sort_by_key(|bank| bank.slot());
    let bank_6 = frozen_banks.get(6).unwrap();
    assert_eq!(bank_6.slot(), 6);

    ReplayStage::adopt_on_chain_tower_if_behind(
        &my_vote_pubkey,
        ancestors,
        &frozen_banks,
        &mut tower,
        &mut progress,
        bank_6,
        &bank_forks,
    );

    // slot 3 should now pass the threshold check but be locked out.
    let fork_stats_3 = progress.get_fork_stats(3).unwrap();
    assert!(fork_stats_3.vote_threshold.is_empty());
    assert!(fork_stats_3.is_locked_out);
    assert!(fork_stats_3.computed);
    // slot 4 should be untouched since it is yet to be computed.
    let fork_stats_4 = progress.get_fork_stats(4).unwrap();
    assert!(!fork_stats_4.is_locked_out);
    assert!(!fork_stats_4.computed);
}

#[test]
fn test_tower_load_missing() {
    let tower_file = tempdir().unwrap().keep();
    let tower_storage = FileTowerStorage::new(tower_file);
    let node_pubkey = Pubkey::new_unique();
    let vote_account = Pubkey::new_unique();
    let tree = tr(0) / (tr(1) / (tr(3) / (tr(4))) / (tr(2) / (tr(5) / (tr(6)))));
    let generate_votes = |pubkeys: Vec<Pubkey>| {
        pubkeys
            .into_iter()
            .zip(iter::once(vec![0, 1, 2, 5, 6]).chain(iter::repeat_n(vec![0, 1, 3, 4], 2)))
            .collect()
    };
    let (vote_simulator, _blockstore) =
        setup_forks_from_tree(tree, 3, Some(Box::new(generate_votes)));
    let bank_forks = vote_simulator.bank_forks;

    let tower =
        ReplayStage::load_tower(&tower_storage, &node_pubkey, &vote_account, &bank_forks).unwrap();
    let expected_tower = Tower::new_for_tests(VOTE_THRESHOLD_DEPTH, VOTE_THRESHOLD_SIZE);
    assert_eq!(tower.vote_state, expected_tower.vote_state);
    assert_eq!(tower.node_pubkey, node_pubkey);
}

#[test]
fn test_tower_load() {
    let tower_file = tempdir().unwrap().keep();
    let tower_storage = FileTowerStorage::new(tower_file);
    let node_keypair = Keypair::new();
    let node_pubkey = node_keypair.pubkey();
    let vote_account = Pubkey::new_unique();
    let tree = tr(0) / (tr(1) / (tr(3) / (tr(4))) / (tr(2) / (tr(5) / (tr(6)))));
    let generate_votes = |pubkeys: Vec<Pubkey>| {
        pubkeys
            .into_iter()
            .zip(iter::once(vec![0, 1, 2, 5, 6]).chain(iter::repeat_n(vec![0, 1, 3, 4], 2)))
            .collect()
    };
    let (vote_simulator, _blockstore) =
        setup_forks_from_tree(tree, 3, Some(Box::new(generate_votes)));
    let bank_forks = vote_simulator.bank_forks;
    let expected_tower = Tower::new_random(node_pubkey);
    expected_tower.save(&tower_storage, &node_keypair).unwrap();

    let tower =
        ReplayStage::load_tower(&tower_storage, &node_pubkey, &vote_account, &bank_forks).unwrap();
    assert_eq!(tower.vote_state, expected_tower.vote_state);
    assert_eq!(tower.node_pubkey, expected_tower.node_pubkey);
}

#[test]
fn test_initialize_progress_and_fork_choice_with_duplicates() {
    agave_logger::setup();
    let GenesisConfigInfo {
        mut genesis_config, ..
    } = create_genesis_config(123);

    let ticks_per_slot = 1;
    genesis_config.ticks_per_slot = ticks_per_slot;
    let (ledger_path, blockhash) =
        solana_ledger::create_new_tmp_ledger_auto_delete!(&genesis_config);
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();

    /*
      Bank forks with:
           slot 0
             |
           slot 1 -> Duplicate before restart, the restart slot
             |
           slot 2
             |
           slot 3 -> Duplicate before restart, artificially rooted
             |
           slot 4 -> Duplicate before restart, artificially rooted
             |
           slot 5 -> Duplicate before restart
             |
           slot 6
    */

    let mut last_hash = blockhash;
    for i in 0..6 {
        last_hash =
            fill_blockstore_slot_with_ticks(&blockstore, ticks_per_slot, i + 1, i, last_hash);
    }
    // Artificially root 3 and 4
    blockstore.set_roots([3, 4].iter()).unwrap();

    // Set up bank0
    let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis_config));
    let bank0 = bank_forks.read().unwrap().get_with_scheduler(0).unwrap();
    let shred_version = compute_shred_version(&genesis_config.hash(), None);
    let replay_tx_thread_pool = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .thread_name(|i| format!("solReplayTx{i:02}"))
        .build()
        .expect("new rayon threadpool");

    process_bank_0(
        &bank0,
        shred_version,
        &blockstore,
        &replay_tx_thread_pool,
        &ProcessOptions::default(),
        None,
        None,
        &MigrationStatus::default(),
    )
    .unwrap();

    // Mark block 1, 3, 4, 5 as duplicate
    blockstore.store_duplicate_slot(1, vec![], vec![]).unwrap();
    blockstore.store_duplicate_slot(3, vec![], vec![]).unwrap();
    blockstore.store_duplicate_slot(4, vec![], vec![]).unwrap();
    blockstore.store_duplicate_slot(5, vec![], vec![]).unwrap();

    let bank1_child =
        Bank::new_from_parent(bank0.clone_without_scheduler(), SlotLeader::default(), 1);
    let bank1 = bank_forks.write().unwrap().insert(bank1_child);
    confirm_full_slot(
        &blockstore,
        &bank1,
        shred_version,
        &replay_tx_thread_pool,
        &ProcessOptions::default(),
        &mut ConfirmationProgress::new(bank0.last_blockhash()),
        None,
        None,
        &mut ExecuteTimings::default(),
        &MigrationStatus::default(),
    )
    .unwrap();

    bank_forks.write().unwrap().set_root(1, None, None);

    let leader_schedule_cache = LeaderScheduleCache::new_from_bank(&bank1);

    // process_blockstore_from_root() from slot 1 onwards
    blockstore_processor::process_blockstore_from_root(
        &blockstore,
        &bank_forks,
        shred_version,
        &leader_schedule_cache,
        &ProcessOptions::default(),
        None,
        None,
        None, // snapshot_controller
    )
    .unwrap();

    assert_eq!(bank_forks.read().unwrap().root(), 4);

    // Verify that fork choice can be initialized and that the root is not marked duplicate
    let (_progress, fork_choice) =
        ReplayStage::initialize_progress_and_fork_choice_with_locked_bank_forks(
            &bank_forks,
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &blockstore,
        );

    let bank_forks = bank_forks.read().unwrap();
    // 4 (the artificial root) is the tree root and no longer duplicate
    assert_eq!(fork_choice.tree_root().0, 4);
    assert!(
        fork_choice
            .is_candidate(&(4, bank_forks.bank_hash(4).unwrap()))
            .unwrap()
    );

    // 5 is still considered duplicate, so it is not a valid fork choice candidate
    assert!(
        !fork_choice
            .is_candidate(&(5, bank_forks.bank_hash(5).unwrap()))
            .unwrap()
    );
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

#[test]
#[should_panic(expected = "Additional duplicate confirmed notification for slot 6")]
fn test_mark_slots_duplicate_confirmed() {
    let generate_votes = |pubkeys: Vec<Pubkey>| {
        pubkeys
            .into_iter()
            .zip(iter::once(vec![0, 1, 2, 5, 6]).chain(iter::repeat_n(vec![0, 1, 3, 4], 2)))
            .collect()
    };
    let tree = tr(0) / (tr(1) / (tr(3) / (tr(4))) / (tr(2) / (tr(5) / (tr(6)))));
    let (vote_simulator, blockstore) =
        setup_forks_from_tree(tree, 3, Some(Box::new(generate_votes)));
    let VoteSimulator {
        bank_forks,
        mut tbft_structs,
        mut progress,
        ..
    } = vote_simulator;

    let (ancestor_hashes_replay_update_sender, _) = bounded(1024);
    let mut duplicate_confirmed_slots = DuplicateConfirmedSlots::default();
    let bank_hash_0 = bank_forks.read().unwrap().bank_hash(0).unwrap();
    bank_forks.write().unwrap().set_root(1, None, None);

    // Mark 0 as duplicate confirmed, should fail as it is 0 < root
    let confirmed_slots = [(0, bank_hash_0)];
    ReplayStage::mark_slots_duplicate_confirmed(
        &confirmed_slots,
        &blockstore,
        &bank_forks,
        &mut progress,
        &mut DuplicateSlotsTracker::default(),
        &mut tbft_structs.heaviest_subtree_fork_choice,
        &mut EpochSlotsFrozenSlots::default(),
        &mut DuplicateSlotsToRepair::default(),
        &ancestor_hashes_replay_update_sender,
        &mut PurgeRepairSlotCounter::default(),
        &mut duplicate_confirmed_slots,
    );

    assert!(!duplicate_confirmed_slots.contains_key(&0));

    // Mark 5 as duplicate confirmed, should succeed
    let bank_hash_5 = bank_forks.read().unwrap().bank_hash(5).unwrap();
    let confirmed_slots = [(5, bank_hash_5)];

    ReplayStage::mark_slots_duplicate_confirmed(
        &confirmed_slots,
        &blockstore,
        &bank_forks,
        &mut progress,
        &mut DuplicateSlotsTracker::default(),
        &mut tbft_structs.heaviest_subtree_fork_choice,
        &mut EpochSlotsFrozenSlots::default(),
        &mut DuplicateSlotsToRepair::default(),
        &ancestor_hashes_replay_update_sender,
        &mut PurgeRepairSlotCounter::default(),
        &mut duplicate_confirmed_slots,
    );

    assert_eq!(*duplicate_confirmed_slots.get(&5).unwrap(), bank_hash_5);
    assert!(
        tbft_structs
            .heaviest_subtree_fork_choice
            .is_duplicate_confirmed(&(5, bank_hash_5))
            .unwrap_or(false)
    );

    // Mark 5 and 6 as duplicate confirmed, should succeed
    let bank_hash_6 = bank_forks.read().unwrap().bank_hash(6).unwrap();
    let confirmed_slots = [(5, bank_hash_5), (6, bank_hash_6)];

    ReplayStage::mark_slots_duplicate_confirmed(
        &confirmed_slots,
        &blockstore,
        &bank_forks,
        &mut progress,
        &mut DuplicateSlotsTracker::default(),
        &mut tbft_structs.heaviest_subtree_fork_choice,
        &mut EpochSlotsFrozenSlots::default(),
        &mut DuplicateSlotsToRepair::default(),
        &ancestor_hashes_replay_update_sender,
        &mut PurgeRepairSlotCounter::default(),
        &mut duplicate_confirmed_slots,
    );

    assert_eq!(*duplicate_confirmed_slots.get(&5).unwrap(), bank_hash_5);
    assert!(
        tbft_structs
            .heaviest_subtree_fork_choice
            .is_duplicate_confirmed(&(5, bank_hash_5))
            .unwrap_or(false)
    );
    assert_eq!(*duplicate_confirmed_slots.get(&6).unwrap(), bank_hash_6);
    assert!(
        tbft_structs
            .heaviest_subtree_fork_choice
            .is_duplicate_confirmed(&(6, bank_hash_6))
            .unwrap_or(false)
    );

    // Mark 6 as duplicate confirmed again with a different hash, should panic
    let confirmed_slots = [(6, Hash::new_unique())];
    ReplayStage::mark_slots_duplicate_confirmed(
        &confirmed_slots,
        &blockstore,
        &bank_forks,
        &mut progress,
        &mut DuplicateSlotsTracker::default(),
        &mut tbft_structs.heaviest_subtree_fork_choice,
        &mut EpochSlotsFrozenSlots::default(),
        &mut DuplicateSlotsToRepair::default(),
        &ancestor_hashes_replay_update_sender,
        &mut PurgeRepairSlotCounter::default(),
        &mut duplicate_confirmed_slots,
    );
}

#[test_case(true ; "same_batch")]
#[test_case(false ; "seperate_batches")]
#[should_panic(expected = "Additional duplicate confirmed notification for slot 6")]
fn test_process_duplicate_confirmed_slots(same_batch: bool) {
    let generate_votes = |pubkeys: Vec<Pubkey>| {
        pubkeys
            .into_iter()
            .zip(iter::once(vec![0, 1, 2, 5, 6]).chain(iter::repeat_n(vec![0, 1, 3, 4], 2)))
            .collect()
    };
    let tree = tr(0) / (tr(1) / (tr(3) / (tr(4))) / (tr(2) / (tr(5) / (tr(6)))));
    let (vote_simulator, blockstore) =
        setup_forks_from_tree(tree, 3, Some(Box::new(generate_votes)));
    let VoteSimulator {
        bank_forks,
        mut tbft_structs,
        progress,
        ..
    } = vote_simulator;

    let (ancestor_hashes_replay_update_sender, _) = bounded(1024);
    let (sender, receiver) = bounded(1024);
    let mut duplicate_confirmed_slots = DuplicateConfirmedSlots::default();
    let bank_hash_0 = bank_forks.read().unwrap().bank_hash(0).unwrap();
    bank_forks.write().unwrap().set_root(1, None, None);

    // Mark 0 as duplicate confirmed, should fail as it is 0 < root
    sender.send(vec![(0, bank_hash_0)]).unwrap();

    ReplayStage::process_duplicate_confirmed_slots(
        &receiver,
        &blockstore,
        &mut DuplicateSlotsTracker::default(),
        &mut duplicate_confirmed_slots,
        &mut EpochSlotsFrozenSlots::default(),
        &bank_forks,
        &progress,
        &mut tbft_structs.heaviest_subtree_fork_choice,
        &mut DuplicateSlotsToRepair::default(),
        &ancestor_hashes_replay_update_sender,
        &mut PurgeRepairSlotCounter::default(),
    );

    assert!(!duplicate_confirmed_slots.contains_key(&0));

    // Mark 5 as duplicate confirmed, should succed
    let bank_hash_5 = bank_forks.read().unwrap().bank_hash(5).unwrap();
    sender.send(vec![(5, bank_hash_5)]).unwrap();

    ReplayStage::process_duplicate_confirmed_slots(
        &receiver,
        &blockstore,
        &mut DuplicateSlotsTracker::default(),
        &mut duplicate_confirmed_slots,
        &mut EpochSlotsFrozenSlots::default(),
        &bank_forks,
        &progress,
        &mut tbft_structs.heaviest_subtree_fork_choice,
        &mut DuplicateSlotsToRepair::default(),
        &ancestor_hashes_replay_update_sender,
        &mut PurgeRepairSlotCounter::default(),
    );

    assert_eq!(*duplicate_confirmed_slots.get(&5).unwrap(), bank_hash_5);
    assert!(
        tbft_structs
            .heaviest_subtree_fork_choice
            .is_duplicate_confirmed(&(5, bank_hash_5))
            .unwrap_or(false)
    );

    // Mark 5 and 6 as duplicate confirmed, should succeed
    let bank_hash_6 = bank_forks.read().unwrap().bank_hash(6).unwrap();
    if same_batch {
        sender
            .send(vec![(5, bank_hash_5), (6, bank_hash_6)])
            .unwrap();
    } else {
        sender.send(vec![(5, bank_hash_5)]).unwrap();
        sender.send(vec![(6, bank_hash_6)]).unwrap();
    }

    ReplayStage::process_duplicate_confirmed_slots(
        &receiver,
        &blockstore,
        &mut DuplicateSlotsTracker::default(),
        &mut duplicate_confirmed_slots,
        &mut EpochSlotsFrozenSlots::default(),
        &bank_forks,
        &progress,
        &mut tbft_structs.heaviest_subtree_fork_choice,
        &mut DuplicateSlotsToRepair::default(),
        &ancestor_hashes_replay_update_sender,
        &mut PurgeRepairSlotCounter::default(),
    );

    assert_eq!(*duplicate_confirmed_slots.get(&5).unwrap(), bank_hash_5);
    assert!(
        tbft_structs
            .heaviest_subtree_fork_choice
            .is_duplicate_confirmed(&(5, bank_hash_5))
            .unwrap_or(false)
    );
    assert_eq!(*duplicate_confirmed_slots.get(&6).unwrap(), bank_hash_6);
    assert!(
        tbft_structs
            .heaviest_subtree_fork_choice
            .is_duplicate_confirmed(&(6, bank_hash_6))
            .unwrap_or(false)
    );

    // Mark 6 as duplicate confirmed again with a different hash, should panic
    sender.send(vec![(6, Hash::new_unique())]).unwrap();

    ReplayStage::process_duplicate_confirmed_slots(
        &receiver,
        &blockstore,
        &mut DuplicateSlotsTracker::default(),
        &mut duplicate_confirmed_slots,
        &mut EpochSlotsFrozenSlots::default(),
        &bank_forks,
        &progress,
        &mut tbft_structs.heaviest_subtree_fork_choice,
        &mut DuplicateSlotsToRepair::default(),
        &ancestor_hashes_replay_update_sender,
        &mut PurgeRepairSlotCounter::default(),
    );
}
