//! Code used by the integration tests in the `local-cluster/tests` folder.
//!
//! According to the cargo documentation, integration tests are compiled as individual crates.
//!
//!   https://doc.rust-lang.org/book/ch11-03-test-organization.html#the-tests-directory
//!
//! Putting shared code into the `tests/` folder, causes it to be recompiled for every test, rather
//! than being compiled once when it is part of the main crate.  And, at the same time, unused code
//! analysis is broken, as it is literally included.  If a shared function is not used in one of the
//! integration tests, it will be flagged as unused, when compiling this test crate.

use {
    crate::{
        cluster::{Cluster, ClusterValidatorInfo},
        cluster_tests,
        local_cluster::{ClusterConfig, LocalCluster},
        validator_configs::*,
    },
    log::*,
    solana_account::AccountSharedData,
    solana_accounts_db::utils::create_accounts_run_and_snapshot_dirs,
    solana_clock::{
        self as clock, Slot, DEFAULT_MS_PER_SLOT, DEFAULT_TICKS_PER_SLOT,
        NUM_CONSECUTIVE_LEADER_SLOTS,
    },
    solana_core::{
        consensus::{tower_storage::FileTowerStorage, Tower, SWITCH_FORK_THRESHOLD},
        snapshot_packager_service::SnapshotPackagerService,
        validator::{is_snapshot_config_valid, ValidatorConfig},
    },
    solana_gossip::gossip_service::discover_validators,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_ledger::{
        ancestor_iterator::AncestorIterator,
        blockstore::{Blockstore, PurgeType},
        blockstore_meta::DuplicateSlotProof,
        blockstore_options::{AccessType, BlockstoreOptions},
        leader_schedule::{FixedSchedule, IdentityKeyedLeaderSchedule, LeaderSchedule},
    },
    solana_native_token::LAMPORTS_PER_SOL,
    solana_pubkey::Pubkey,
    solana_rpc_client::rpc_client::RpcClient,
    solana_runtime::{snapshot_config::SnapshotConfig, snapshot_utils::SnapshotInterval},
    solana_signer::Signer,
    solana_streamer::socket::SocketAddrSpace,
    solana_turbine::broadcast_stage::BroadcastStageType,
    solana_votor::voting_service::{AlpenglowPortOverride, VotingServiceOverride},
    static_assertions,
    std::{
        collections::{HashMap, HashSet},
        fs, iter,
        net::SocketAddr,
        num::{NonZeroU64, NonZeroUsize},
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread::sleep,
        time::Duration,
    },
    tempfile::TempDir,
};

pub const RUST_LOG_FILTER: &str =
    "error,solana_core::replay_stage=warn,solana_local_cluster=info,local_cluster=info";

pub const AG_DEBUG_LOG_FILTER: &str =
    "error,solana_core::replay_stage=info,solana_local_cluster=info,local_cluster=info,\
     solana_core::block_creation_loop=trace,solana_votor=trace,\
     solana_votor::vote_history_storage=info,solana_core::validator=info";
pub const DEFAULT_NODE_STAKE: u64 = 10 * LAMPORTS_PER_SOL;

pub fn last_vote_in_tower(tower_path: &Path, node_pubkey: &Pubkey) -> Option<(Slot, Hash)> {
    restore_tower(tower_path, node_pubkey).map(|tower| tower.last_voted_slot_hash().unwrap())
}

pub fn last_root_in_tower(tower_path: &Path, node_pubkey: &Pubkey) -> Option<Slot> {
    restore_tower(tower_path, node_pubkey).map(|tower| tower.root())
}

pub fn restore_tower(tower_path: &Path, node_pubkey: &Pubkey) -> Option<Tower> {
    let file_tower_storage = FileTowerStorage::new(tower_path.to_path_buf());

    let tower = Tower::restore(&file_tower_storage, node_pubkey);
    if let Err(tower_err) = tower {
        if tower_err.is_file_missing() {
            return None;
        } else {
            panic!("tower restore failed...: {tower_err:?}");
        }
    }
    // actually saved tower must have at least one vote.
    Tower::restore(&file_tower_storage, node_pubkey).ok()
}

pub fn remove_tower_if_exists(tower_path: &Path, node_pubkey: &Pubkey) {
    let file_tower_storage = FileTowerStorage::new(tower_path.to_path_buf());
    let filename = file_tower_storage.filename(node_pubkey);
    if filename.exists() {
        fs::remove_file(file_tower_storage.filename(node_pubkey)).unwrap();
    }
}

pub fn remove_tower(tower_path: &Path, node_pubkey: &Pubkey) {
    let file_tower_storage = FileTowerStorage::new(tower_path.to_path_buf());
    fs::remove_file(file_tower_storage.filename(node_pubkey)).unwrap();
}

pub fn open_blockstore(ledger_path: &Path) -> Blockstore {
    Blockstore::open_with_options(
        ledger_path,
        BlockstoreOptions {
            access_type: AccessType::Primary,
            recovery_mode: None,
            enforce_ulimit_nofile: true,
            ..BlockstoreOptions::default()
        },
    )
    // Fall back on Secondary if Primary fails; Primary will fail if
    // a handle to Blockstore is being held somewhere else
    .unwrap_or_else(|_| {
        Blockstore::open_with_options(
            ledger_path,
            BlockstoreOptions {
                access_type: AccessType::Secondary,
                recovery_mode: None,
                enforce_ulimit_nofile: true,
                ..BlockstoreOptions::default()
            },
        )
        .unwrap_or_else(|e| {
            panic!("Failed to open ledger at {ledger_path:?}, err: {e}");
        })
    })
}

pub fn purge_slots_with_count(blockstore: &Blockstore, start_slot: Slot, slot_count: Slot) {
    blockstore.purge_from_next_slots(start_slot, start_slot + slot_count - 1);
    blockstore.purge_slots(start_slot, start_slot + slot_count - 1, PurgeType::Exact);
}

// Fetches the last vote in the tower, blocking until it has also appeared in blockstore.
// Fails if tower is empty
pub fn wait_for_last_vote_in_tower_to_land_in_ledger(
    ledger_path: &Path,
    node_pubkey: &Pubkey,
) -> Option<Slot> {
    last_vote_in_tower(ledger_path, node_pubkey).map(|(last_vote, _)| {
        loop {
            // We reopen in a loop to make sure we get updates
            let blockstore = open_blockstore(ledger_path);
            if blockstore.is_full(last_vote) {
                break;
            }
            sleep(Duration::from_millis(100));
        }
        last_vote
    })
}

/// Waits roughly 10 seconds for duplicate proof to appear in blockstore at `dup_slot`. Returns proof if found.
pub fn wait_for_duplicate_proof(ledger_path: &Path, dup_slot: Slot) -> Option<DuplicateSlotProof> {
    for _ in 0..10 {
        let duplicate_fork_validator_blockstore = open_blockstore(ledger_path);
        if let Some((found_dup_slot, found_duplicate_proof)) =
            duplicate_fork_validator_blockstore.get_first_duplicate_proof()
        {
            if found_dup_slot == dup_slot {
                return Some(found_duplicate_proof);
            };
        }

        sleep(Duration::from_millis(1000));
    }
    None
}

/// Copy blocks from `source` blockstore to `dest` blockstore
/// Set `is_trusted` to avoid sanity checks typically performed by Blockstore::insert_shreds()
pub fn copy_blocks(end_slot: Slot, source: &Blockstore, dest: &Blockstore, is_trusted: bool) {
    for slot in std::iter::once(end_slot).chain(AncestorIterator::new(end_slot, source)) {
        let source_meta = source.meta(slot).unwrap().unwrap();
        assert!(source_meta.is_full());

        let shreds = source.get_data_shreds_for_slot(slot, 0).unwrap();
        dest.insert_shreds(shreds, None, is_trusted).unwrap();

        let dest_meta = dest.meta(slot).unwrap().unwrap();
        assert!(dest_meta.is_full());
        assert_eq!(dest_meta.last_index, source_meta.last_index);
    }
}

/// Computes the numbr of milliseconds `num_blocks` blocks will take given
/// each slot contains `ticks_per_slot`
pub fn ms_for_n_slots(num_blocks: u64, ticks_per_slot: u64) -> u64 {
    (ticks_per_slot * DEFAULT_MS_PER_SLOT * num_blocks).div_ceil(DEFAULT_TICKS_PER_SLOT)
}

/// Implements a test scenario that creates a network partition by killing validator nodes.
///
/// # Arguments
/// * `stakes_to_kill` - Validators to remove from the network, where each tuple contains:
///   * First element (usize): The stake weight/size of the validator
///   * Second element (usize): The number of slots assigned to the validator
/// * `alive_stakes` - Validators to keep alive, where each tuple contains:
///   * First element (usize): The stake weight/size of the validator
///   * Second element (usize): The number of slots assigned to the validator
/// * `ticks_per_slot` - Optional override for the default ticks per slot
/// * `partition_context` - Test-specific context object that will be passed to callbacks
/// * `on_partition_start` - Callback executed when the partition begins
/// * `on_before_partition_resolved` - Callback executed right before the partition is resolved
/// * `on_partition_resolved` - Callback executed after the partition is resolved
///
/// This function simulates a network partition by killing specified validator nodes,
/// waiting for a period, resolving the partition, and then verifying the network
/// can recover and reach consensus. The IS_ALPENGLOW parameter determines whether
/// to use Alpenglow-specific cluster initialization.
fn run_kill_partition_switch_threshold_impl<C, const IS_ALPENGLOW: bool>(
    stakes_to_kill: &[(usize, usize)],
    alive_stakes: &[(usize, usize)],
    ticks_per_slot: Option<u64>,
    partition_context: C,
    on_partition_start: impl Fn(&mut LocalCluster, &[Pubkey], Vec<ClusterValidatorInfo>, &mut C),
    on_before_partition_resolved: impl Fn(&mut LocalCluster, &mut C),
    on_partition_resolved: impl Fn(&mut LocalCluster, &mut C),
) {
    // Needs to be at least 1/3 or there will be no overlap
    // with the confirmation supermajority 2/3
    static_assertions::const_assert!(SWITCH_FORK_THRESHOLD >= 1f64 / 3f64);
    info!("stakes_to_kill: {stakes_to_kill:?}, alive_stakes: {alive_stakes:?}");

    // This test:
    // 1) Spins up three partitions
    // 2) Kills the first partition with the stake `failures_stake`
    // 5) runs `on_partition_resolved`
    let partitions: Vec<(usize, usize)> = stakes_to_kill
        .iter()
        .cloned()
        .chain(alive_stakes.iter().cloned())
        .collect();

    let stake_partitions: Vec<usize> = partitions.iter().map(|(stake, _)| *stake).collect();
    let num_slots_per_validator: Vec<usize> =
        partitions.iter().map(|(_, num_slots)| *num_slots).collect();

    let (leader_schedule, validator_keys) =
        create_custom_leader_schedule_with_random_keys(&num_slots_per_validator);

    info!(
        "Validator ids: {:?}",
        validator_keys
            .iter()
            .map(|k| k.pubkey())
            .collect::<Vec<_>>()
    );
    let validator_pubkeys: Vec<Pubkey> = validator_keys.iter().map(|k| k.pubkey()).collect();
    let on_partition_start = |cluster: &mut LocalCluster, partition_context: &mut C| {
        let dead_validator_infos: Vec<ClusterValidatorInfo> = validator_pubkeys
            [0..stakes_to_kill.len()]
            .iter()
            .map(|validator_to_kill| {
                info!("Killing validator with id: {validator_to_kill}");
                cluster.exit_node(validator_to_kill)
            })
            .collect();
        on_partition_start(
            cluster,
            &validator_pubkeys,
            dead_validator_infos,
            partition_context,
        );
    };
    run_cluster_partition::<C>(
        &stake_partitions,
        Some((leader_schedule, validator_keys)),
        partition_context,
        on_partition_start,
        on_before_partition_resolved,
        on_partition_resolved,
        ticks_per_slot,
        vec![],
        IS_ALPENGLOW,
    )
}

pub fn run_kill_partition_switch_threshold_alpenglow<C>(
    stakes_to_kill: &[(usize, usize)],
    alive_stakes: &[(usize, usize)],
    ticks_per_slot: Option<u64>,
    partition_context: C,
    on_partition_start: impl Fn(&mut LocalCluster, &[Pubkey], Vec<ClusterValidatorInfo>, &mut C),
    on_before_partition_resolved: impl Fn(&mut LocalCluster, &mut C),
    on_partition_resolved: impl Fn(&mut LocalCluster, &mut C),
) {
    run_kill_partition_switch_threshold_impl::<C, true>(
        stakes_to_kill,
        alive_stakes,
        ticks_per_slot,
        partition_context,
        on_partition_start,
        on_before_partition_resolved,
        on_partition_resolved,
    )
}

pub fn run_kill_partition_switch_threshold<C>(
    stakes_to_kill: &[(usize, usize)],
    alive_stakes: &[(usize, usize)],
    ticks_per_slot: Option<u64>,
    partition_context: C,
    on_partition_start: impl Fn(&mut LocalCluster, &[Pubkey], Vec<ClusterValidatorInfo>, &mut C),
    on_before_partition_resolved: impl Fn(&mut LocalCluster, &mut C),
    on_partition_resolved: impl Fn(&mut LocalCluster, &mut C),
) {
    run_kill_partition_switch_threshold_impl::<C, false>(
        stakes_to_kill,
        alive_stakes,
        ticks_per_slot,
        partition_context,
        on_partition_start,
        on_before_partition_resolved,
        on_partition_resolved,
    )
}

pub fn create_custom_leader_schedule(
    validator_key_to_slots: impl Iterator<Item = (Pubkey, usize)>,
) -> LeaderSchedule {
    let leader_schedule: Vec<_> = validator_key_to_slots
        .flat_map(|(pubkey, num_slots)| {
            // Ensure that the number of slots is a multiple of NUM_CONSECUTIVE_LEADER_SLOTS
            // Because we only check leadership every NUM_CONSECUTIVE_LEADER_SLOTS slots, for
            // example, you can have [(pubkey_A, 70), (pubkey_B, 30)], A will happily produce
            // block 70 and 71 because it is the leader for block 68, but when B gets the shred
            // it check leadership for block 70 and 71, it will see that it is the leader, so the
            // shreds from A will be ignored.
            assert!(num_slots % (NUM_CONSECUTIVE_LEADER_SLOTS as usize) == 0);
            std::iter::repeat_n(pubkey, num_slots)
        })
        .collect();

    info!("leader_schedule: {}", leader_schedule.len());
    Box::new(IdentityKeyedLeaderSchedule::new_from_schedule(
        leader_schedule,
    ))
}

pub fn create_custom_leader_schedule_with_random_keys(
    validator_num_slots: &[usize],
) -> (LeaderSchedule, Vec<Arc<Keypair>>) {
    let validator_keys: Vec<_> = iter::repeat_with(|| Arc::new(Keypair::new()))
        .take(validator_num_slots.len())
        .collect();
    let leader_schedule = create_custom_leader_schedule(
        validator_keys
            .iter()
            .map(|k| k.pubkey())
            .zip(validator_num_slots.iter().cloned()),
    );
    (leader_schedule, validator_keys)
}

/// Simulates a network partition test scenario by creating a cluster, triggering a partition,
/// allowing the partition to heal, and then verifying the network's ability to recover and
/// achieve consensus after the partition is resolved.
///
/// This function:
/// 1. Creates a local cluster with nodes configured according to the provided stakes
/// 2. Induces a network partition by disabling communication between validators
/// 3. Runs the partition for a predetermined duration
/// 4. Resolves the partition by re-enabling communication
/// 5. Verifies the network can recover and continue to make progress
///
/// # Arguments
/// * `partitions` - A slice of partition configurations, where each usize represents a validator's
///   stake weight. This determines the relative voting power of each node in the network.
/// * `leader_schedule` - An option that specifies whether the cluster should run with a fixed,
///   predetermined leader schedule. If provided, the partition will last for one complete
///   iteration of the leader schedule.
/// * `context` - A user-defined context object that is passed to the callback functions.
/// * `on_partition_start` - Callback function that runs when the partition begins. Can be used
///   to perform custom actions or checks at the start of the partition.
/// * `on_before_partition_resolved` - Callback function that runs just before the partition
///   is resolved. Can be used to verify partition state or prepare for resolution.
/// * `on_partition_resolved` - Callback function that runs after the partition is resolved and
///   the network has had time to recover. Can be used to verify recovery.
/// * `ticks_per_slot` - Optional override for the default ticks per slot. Controls the
///   rate at which slots advance in the cluster.
/// * `additional_accounts` - Additional accounts to be added to the genesis configuration.
/// * `is_alpenglow` - Boolean flag indicating whether to initialize the `LocalCluster` in Alpenglow
///   mode.
#[allow(clippy::cognitive_complexity)]
pub fn run_cluster_partition<C>(
    partitions: &[usize],
    leader_schedule: Option<(LeaderSchedule, Vec<Arc<Keypair>>)>,
    mut context: C,
    on_partition_start: impl FnOnce(&mut LocalCluster, &mut C),
    on_before_partition_resolved: impl FnOnce(&mut LocalCluster, &mut C),
    on_partition_resolved: impl FnOnce(&mut LocalCluster, &mut C),
    ticks_per_slot: Option<u64>,
    additional_accounts: Vec<(Pubkey, AccountSharedData)>,
    is_alpenglow: bool,
) {
    solana_logger::setup_with_default(RUST_LOG_FILTER);
    info!("PARTITION_TEST!");
    let num_nodes = partitions.len();
    let node_stakes: Vec<_> = partitions
        .iter()
        .map(|stake_weight| 100 * *stake_weight as u64)
        .collect();
    assert_eq!(node_stakes.len(), num_nodes);
    let mint_lamports = node_stakes.iter().sum::<u64>() * 2;
    let turbine_disabled = Arc::new(AtomicBool::new(false));
    let mut validator_config = ValidatorConfig {
        turbine_disabled: turbine_disabled.clone(),
        ..ValidatorConfig::default_for_test()
    };

    let (validator_keys, partition_duration): (Vec<_>, Duration) = {
        if let Some((leader_schedule, validator_keys)) = leader_schedule {
            assert_eq!(validator_keys.len(), num_nodes);
            let num_slots_per_rotation = leader_schedule.num_slots() as u64;
            let fixed_schedule = FixedSchedule {
                leader_schedule: Arc::new(leader_schedule),
            };
            validator_config.fixed_leader_schedule = Some(fixed_schedule);
            (
                validator_keys,
                // partition for the duration of one full iteration of the  leader schedule
                Duration::from_millis(num_slots_per_rotation * clock::DEFAULT_MS_PER_SLOT),
            )
        } else {
            (
                iter::repeat_with(|| Arc::new(Keypair::new()))
                    .take(partitions.len())
                    .collect(),
                Duration::from_secs(10),
            )
        }
    };

    let slots_per_epoch = 2048;
    let alpenglow_port_override = AlpenglowPortOverride::default();
    let validator_configs = make_identical_validator_configs(&validator_config, num_nodes)
        .into_iter()
        .map(|mut config| {
            config.voting_service_test_override = Some(VotingServiceOverride {
                additional_listeners: vec![],
                alpenglow_port_override: alpenglow_port_override.clone(),
            });
            config
        })
        .collect();
    let mut config = ClusterConfig {
        mint_lamports,
        node_stakes,
        validator_configs,
        validator_keys: Some(
            validator_keys
                .into_iter()
                .zip(iter::repeat_with(|| true))
                .collect(),
        ),
        slots_per_epoch,
        stakers_slot_offset: slots_per_epoch,
        skip_warmup_slots: true,
        additional_accounts,
        ticks_per_slot: ticks_per_slot.unwrap_or(DEFAULT_TICKS_PER_SLOT),
        tpu_connection_pool_size: 2,
        ..ClusterConfig::default()
    };

    info!(
        "PARTITION_TEST starting cluster with {:?} partitions slots_per_epoch: {}",
        partitions, config.slots_per_epoch,
    );

    let mut cluster = if is_alpenglow {
        LocalCluster::new_alpenglow(&mut config, SocketAddrSpace::Unspecified)
    } else {
        LocalCluster::new(&mut config, SocketAddrSpace::Unspecified)
    };

    info!("PARTITION_TEST spend_and_verify_all_nodes(), ensure all nodes are caught up");
    cluster_tests::spend_and_verify_all_nodes(
        &cluster.entry_point_info,
        &cluster.funding_keypair,
        num_nodes,
        HashSet::new(),
        SocketAddrSpace::Unspecified,
        &cluster.connection_cache,
    );

    let cluster_nodes = discover_validators(
        &cluster.entry_point_info.gossip().unwrap(),
        num_nodes,
        cluster.entry_point_info.shred_version(),
        SocketAddrSpace::Unspecified,
    )
    .unwrap();

    // Check epochs have correct number of slots
    info!("PARTITION_TEST sleeping until partition starting condition",);
    for node in &cluster_nodes {
        let node_client = RpcClient::new_socket(node.rpc().unwrap());
        let epoch_info = node_client.get_epoch_info().unwrap();
        assert_eq!(epoch_info.slots_in_epoch, slots_per_epoch);
    }

    info!("PARTITION_TEST start partition");
    on_partition_start(&mut cluster, &mut context);
    turbine_disabled.store(true, Ordering::Relaxed);
    // Make all to all votes/certs not able to reach each other by overriding the
    // alpenglow port override to SocketAddr which no one is listening on.
    let blackhole_addr: SocketAddr = solana_net_utils::bind_to_localhost()
        .unwrap()
        .local_addr()
        .unwrap();
    let new_override = HashMap::from_iter(
        cluster_nodes
            .iter()
            .map(|node| (*node.pubkey(), blackhole_addr)),
    );
    alpenglow_port_override.update_override(new_override);
    sleep(partition_duration);

    on_before_partition_resolved(&mut cluster, &mut context);
    info!("PARTITION_TEST remove partition");
    turbine_disabled.store(false, Ordering::Relaxed);
    // Restore the alpenglow port override to the default, so that the nodes can communicate again.
    alpenglow_port_override.clear();

    // Give partitions time to propagate their blocks from during the partition
    // after the partition resolves
    let timeout_duration = Duration::from_secs(10);
    let propagation_duration = partition_duration;
    info!(
        "PARTITION_TEST resolving partition. sleeping {} ms",
        timeout_duration.as_millis()
    );
    sleep(timeout_duration);
    info!(
        "PARTITION_TEST waiting for blocks to propagate after partition {}ms",
        propagation_duration.as_millis()
    );
    sleep(propagation_duration);
    info!("PARTITION_TEST resuming normal operation");
    on_partition_resolved(&mut cluster, &mut context);
}

pub struct ValidatorTestConfig {
    pub validator_keypair: Arc<Keypair>,
    pub validator_config: ValidatorConfig,
    pub in_genesis: bool,
}

pub fn test_faulty_node(
    faulty_node_type: BroadcastStageType,
    node_stakes: Vec<u64>,
    validator_test_configs: Option<Vec<ValidatorTestConfig>>,
    custom_leader_schedule: Option<FixedSchedule>,
) -> (LocalCluster, Vec<Arc<Keypair>>) {
    let num_nodes = node_stakes.len();
    let validator_keys = validator_test_configs
        .as_ref()
        .map(|configs| {
            configs
                .iter()
                .map(|config| (config.validator_keypair.clone(), config.in_genesis))
                .collect()
        })
        .unwrap_or_else(|| {
            let mut validator_keys = Vec::with_capacity(num_nodes);
            validator_keys.resize_with(num_nodes, || (Arc::new(Keypair::new()), true));
            validator_keys
        });

    assert_eq!(node_stakes.len(), num_nodes);
    assert_eq!(validator_keys.len(), num_nodes);

    let fixed_leader_schedule = custom_leader_schedule.unwrap_or_else(|| {
        // Use a fixed leader schedule so that only the faulty node gets leader slots.
        let validator_to_slots = vec![(
            validator_keys[0].0.as_ref().pubkey(),
            solana_clock::DEFAULT_DEV_SLOTS_PER_EPOCH as usize,
        )];
        let leader_schedule = create_custom_leader_schedule(validator_to_slots.into_iter());
        FixedSchedule {
            leader_schedule: Arc::new(leader_schedule),
        }
    });

    let mut validator_configs = validator_test_configs
        .map(|configs| {
            configs
                .into_iter()
                .map(|config| config.validator_config)
                .collect()
        })
        .unwrap_or_else(|| {
            let mut configs = Vec::with_capacity(num_nodes);
            configs.resize_with(num_nodes, ValidatorConfig::default_for_test);
            configs
        });

    // First validator is the bootstrap leader with the malicious broadcast logic.
    validator_configs[0].broadcast_stage_type = faulty_node_type;
    for config in &mut validator_configs {
        config.fixed_leader_schedule = Some(fixed_leader_schedule.clone());
    }

    let mut cluster_config = ClusterConfig {
        mint_lamports: 10_000,
        node_stakes,
        validator_configs,
        validator_keys: Some(validator_keys.clone()),
        skip_warmup_slots: true,
        ..ClusterConfig::default()
    };

    let cluster = LocalCluster::new(&mut cluster_config, SocketAddrSpace::Unspecified);
    let validator_keys: Vec<Arc<Keypair>> = validator_keys
        .into_iter()
        .map(|(keypair, _)| keypair)
        .collect();

    (cluster, validator_keys)
}

pub fn farf_dir() -> PathBuf {
    std::env::var("FARF_DIR")
        .unwrap_or_else(|_| "farf".to_string())
        .into()
}

pub fn generate_account_paths(num_account_paths: usize) -> (Vec<TempDir>, Vec<PathBuf>) {
    let account_storage_dirs: Vec<TempDir> = (0..num_account_paths)
        .map(|_| tempfile::tempdir_in(farf_dir()).unwrap())
        .collect();
    let account_storage_paths: Vec<_> = account_storage_dirs
        .iter()
        .map(|a| create_accounts_run_and_snapshot_dirs(a.path()).unwrap().0)
        .collect();
    (account_storage_dirs, account_storage_paths)
}

pub struct SnapshotValidatorConfig {
    pub bank_snapshots_dir: TempDir,
    pub full_snapshot_archives_dir: TempDir,
    pub incremental_snapshot_archives_dir: TempDir,
    pub account_storage_dirs: Vec<TempDir>,
    pub validator_config: ValidatorConfig,
}

impl SnapshotValidatorConfig {
    pub fn new(
        full_snapshot_archive_interval: SnapshotInterval,
        incremental_snapshot_archive_interval: SnapshotInterval,
        num_account_paths: usize,
    ) -> SnapshotValidatorConfig {
        // Ensure that some snapshots will be created
        assert_ne!(full_snapshot_archive_interval, SnapshotInterval::Disabled);

        // Create the snapshot config
        let _ = fs::create_dir_all(farf_dir());
        let bank_snapshots_dir = tempfile::tempdir_in(farf_dir()).unwrap();
        let full_snapshot_archives_dir = tempfile::tempdir_in(farf_dir()).unwrap();
        let incremental_snapshot_archives_dir = tempfile::tempdir_in(farf_dir()).unwrap();
        let snapshot_config = SnapshotConfig {
            full_snapshot_archive_interval,
            incremental_snapshot_archive_interval,
            full_snapshot_archives_dir: full_snapshot_archives_dir.path().to_path_buf(),
            incremental_snapshot_archives_dir: incremental_snapshot_archives_dir
                .path()
                .to_path_buf(),
            bank_snapshots_dir: bank_snapshots_dir.path().to_path_buf(),
            maximum_full_snapshot_archives_to_retain: NonZeroUsize::new(usize::MAX).unwrap(),
            maximum_incremental_snapshot_archives_to_retain: NonZeroUsize::new(usize::MAX).unwrap(),
            ..SnapshotConfig::default()
        };
        assert!(is_snapshot_config_valid(&snapshot_config));

        // Create the account paths
        let (account_storage_dirs, account_storage_paths) =
            generate_account_paths(num_account_paths);

        // Create the validator config
        let validator_config = ValidatorConfig {
            snapshot_config,
            account_paths: account_storage_paths,
            validator_exit_backpressure: [(
                SnapshotPackagerService::NAME.to_string(),
                Arc::new(AtomicBool::new(false)),
            )]
            .into(),
            ..ValidatorConfig::default_for_test()
        };

        SnapshotValidatorConfig {
            bank_snapshots_dir,
            full_snapshot_archives_dir,
            incremental_snapshot_archives_dir,
            account_storage_dirs,
            validator_config,
        }
    }
}

pub fn setup_snapshot_validator_config(
    snapshot_interval_slots: NonZeroU64,
    num_account_paths: usize,
) -> SnapshotValidatorConfig {
    SnapshotValidatorConfig::new(
        SnapshotInterval::Slots(snapshot_interval_slots),
        SnapshotInterval::Disabled,
        num_account_paths,
    )
}

pub fn save_tower(tower_path: &Path, tower: &Tower, node_keypair: &Keypair) {
    let file_tower_storage = FileTowerStorage::new(tower_path.to_path_buf());
    tower.save(&file_tower_storage, node_keypair).unwrap();
}
