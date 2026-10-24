#![allow(clippy::arithmetic_side_effects)]
#[path = "local_cluster/reward_recovery.rs"]
mod reward_recovery;

use {
    agave_snapshots::{
        SnapshotArchiveKind, SnapshotInterval, paths as snapshot_paths,
        snapshot_archive_info::SnapshotArchiveInfoGetter,
    },
    arc_swap::ArcSwap,
    assert_matches::assert_matches,
    crossbeam_channel::{Receiver, bounded},
    itertools::Itertools,
    log::*,
    rand::seq::SliceRandom,
    serial_test::serial,
    solana_account::AccountSharedData,
    solana_clock::Slot,
    solana_cluster_type::ClusterType,
    solana_commitment_config::CommitmentConfig,
    solana_core::{
        repair::{
            malicious_repair_handler::MaliciousRepairConfig, repair_handler::RepairHandlerType,
        },
        validator::{BlockProductionMethod, BlockVerificationMethod, ValidatorConfig},
    },
    solana_download_utils::download_snapshot_archive,
    solana_epoch_schedule::MINIMUM_SLOTS_PER_EPOCH,
    solana_gossip::{contact_info::Protocol, gossip_service::discover_validators},
    solana_keypair::{Keypair, keypair_from_seed},
    solana_leader_schedule::FixedSchedule,
    solana_ledger::{
        blockstore::Blockstore,
        blockstore_options::{AccessType, BlockstoreOptions},
        shred::{
            DATA_SHREDS_PER_FEC_BLOCK,
            filter::{TurbineMode, TurbineModeKind},
        },
        use_snapshot_archives_at_startup::UseSnapshotArchivesAtStartup,
    },
    solana_local_cluster::{
        cluster::Cluster,
        cluster_tests,
        integration_tests::{
            AG_DEBUG_LOG_FILTER, DEFAULT_NODE_STAKE, RUST_LOG_FILTER, SnapshotValidatorConfig,
            ValidatorKeys, create_custom_leader_schedule_with_random_keys, farf_dir,
            generate_account_paths, run_cluster_partition, setup_snapshot_validator_config,
        },
        local_cluster::{ClusterConfig, DEFAULT_MINT_LAMPORTS, LocalCluster},
        validator_configs::*,
    },
    solana_net_utils::{SocketAddrSpace, sockets::bind_to_localhost_unique},
    solana_poh_config::PohConfig,
    solana_pubkey::Pubkey,
    solana_pubsub_client::pubsub_client::PubsubClient,
    solana_rpc_client::rpc_client::RpcClient,
    solana_rpc_client_api::{
        config::{RpcProgramAccountsConfig, RpcSignatureSubscribeConfig},
        response::RpcSignatureResult,
    },
    solana_runtime::snapshot_utils,
    solana_signer::Signer,
    solana_stake_interface as stake,
    solana_system_interface::program as system_program,
    solana_system_transaction as system_transaction,
    std::{
        collections::{HashMap, HashSet},
        fs, iter,
        net::SocketAddr,
        num::NonZeroU64,
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{Builder, JoinHandle, sleep},
        time::{Duration, Instant},
    },
    strum::{EnumCount, IntoEnumIterator},
};

fn create_test_validator_keys(keypairs: &[&str]) -> Vec<(ValidatorKeys, bool)> {
    keypairs
        .iter()
        .map(|s| {
            (
                ValidatorKeys {
                    node_keypair: Arc::new(Keypair::from_base58_string(s)),
                    vote_keypair: Arc::new(Keypair::new()),
                },
                true,
            )
        })
        .collect()
}
/// Smoke-tests the `new_with_equal_stakes()` startup path for a single-node cluster.
#[test]
#[serial]
fn test_local_cluster_start_and_exit() {
    agave_logger::setup();
    let num_nodes = 1;
    let cluster = LocalCluster::new_with_equal_stakes(
        num_nodes,
        DEFAULT_MINT_LAMPORTS,
        DEFAULT_NODE_STAKE,
        SocketAddrSpace::Unspecified,
    );
    assert_eq!(cluster.validators.len(), num_nodes);
}

/// Baseline multi-node transaction propagation and confirmation across every validator.
/// Later spend tests are narrower; this is the broad cluster-sanity reference point.
#[test]
#[serial]
fn test_spend_and_verify_all_nodes() {
    agave_logger::setup_with_default(RUST_LOG_FILTER);
    let num_nodes = 3;
    info!("test_spend_and_verify_all_nodes with {num_nodes} nodes");
    let local = LocalCluster::new_with_equal_stakes(
        num_nodes,
        DEFAULT_MINT_LAMPORTS,
        DEFAULT_NODE_STAKE,
        SocketAddrSpace::Unspecified,
    );
    cluster_tests::spend_and_verify_all_nodes(
        &local.entry_point_info,
        &local.funding_keypair,
        num_nodes,
        HashSet::new(),
        SocketAddrSpace::Unspecified,
        &cluster_tests::TpuSender::new(),
    );
}

/// Ensures a follower RPC PubSub endpoint emits both receive and processed signature
/// notifications, not just the leader-facing send/confirm path covered elsewhere.
#[test]
#[serial]
fn test_local_cluster_signature_subscribe() {
    agave_logger::setup_with_default(RUST_LOG_FILTER);
    let num_nodes = 2;
    let cluster = LocalCluster::new_with_equal_stakes(
        num_nodes,
        DEFAULT_MINT_LAMPORTS,
        DEFAULT_NODE_STAKE,
        SocketAddrSpace::Unspecified,
    );
    let nodes = cluster.get_node_pubkeys();

    // Get non leader
    let non_bootstrap_id = nodes
        .into_iter()
        .find(|id| id != cluster.entry_point_info.pubkey())
        .unwrap();
    let non_bootstrap_info = cluster.get_contact_info(&non_bootstrap_id).unwrap();

    let rpc_client = cluster
        .build_rpc_client(cluster.entry_point_info.pubkey())
        .unwrap();

    let (blockhash, _) = rpc_client
        .get_latest_blockhash_with_commitment(CommitmentConfig::processed())
        .unwrap();

    let mut transaction = system_transaction::transfer(
        &cluster.funding_keypair,
        &solana_pubkey::new_rand(),
        10,
        blockhash,
    );

    let (mut sig_subscribe_client, receiver) = PubsubClient::signature_subscribe(
        format!("ws://{}", non_bootstrap_info.rpc_pubsub().unwrap()),
        &transaction.signatures[0],
        Some(RpcSignatureSubscribeConfig {
            commitment: Some(CommitmentConfig::processed()),
            enable_received_notification: Some(true),
        }),
    )
    .unwrap();

    let tpu_sender = cluster_tests::TpuSender::new();
    let leader_pubkey = rpc_client
        .get_slot_leader()
        .expect("test_bad_handshake: get_slot_leader");
    let tpu_addr = rpc_client
        .get_cluster_nodes()
        .expect("test_bad_handshake: get_cluster_nodes")
        .into_iter()
        .find(|n| n.pubkey == leader_pubkey.to_string())
        .and_then(|n| n.tpu_quic)
        .or_else(|| {
            cluster
                .get_contact_info(cluster.entry_point_info.pubkey())
                .unwrap()
                .tpu(Protocol::QUIC)
        })
        .expect("test_bad_handshake: leader has no QUIC TPU address");
    tpu_sender
        .send_transaction_with_retries(
            tpu_addr,
            &rpc_client,
            &[&cluster.funding_keypair],
            &mut transaction,
            5,
        )
        .unwrap();

    let mut got_received_notification = false;
    loop {
        let responses: Vec<_> = receiver.try_iter().collect();
        let mut should_break = false;
        for response in responses {
            match response.value {
                RpcSignatureResult::ProcessedSignature(_) => {
                    should_break = true;
                    break;
                }
                RpcSignatureResult::ReceivedSignature(_) => {
                    got_received_notification = true;
                }
            }
        }

        if should_break {
            break;
        }
        sleep(Duration::from_millis(100));
    }

    // If we don't drop the cluster, the blocking web socket service
    // won't return, and the `sig_subscribe_client` won't shut down
    drop(cluster);
    sig_subscribe_client.shutdown().unwrap();
    assert!(got_received_notification);
}
/// Exercises TPU forwarding by sending transactions to a validator that never leads.
/// Other spend tests assume direct leader delivery; this one proves the forwarding path itself.
#[test]
#[serial]
fn test_forwarding() {
    agave_logger::setup_with_default(RUST_LOG_FILTER);
    // Set up a cluster where one node is never the leader, so all txs sent to this node
    // will be have to be forwarded in order to be confirmed
    let mut config = ClusterConfig {
        node_stakes: vec![DEFAULT_NODE_STAKE * 100, DEFAULT_NODE_STAKE],
        mint_lamports: DEFAULT_MINT_LAMPORTS + DEFAULT_NODE_STAKE * 100,
        validator_configs: make_identical_validator_configs(
            &ValidatorConfig::default_for_test(),
            2,
        ),
        ..ClusterConfig::default()
    };

    let cluster = LocalCluster::new(&mut config, SocketAddrSpace::Unspecified);
    let cluster_nodes = discover_validators(
        &cluster.entry_point_info.gossip().unwrap(),
        2,
        cluster.entry_point_info.shred_version(),
        SocketAddrSpace::Unspecified,
    )
    .unwrap();
    assert!(cluster_nodes.len() >= 2);

    let leader_pubkey = *cluster.entry_point_info.pubkey();

    let validator_info = cluster_nodes
        .iter()
        .find(|c| c.pubkey() != &leader_pubkey)
        .unwrap();

    // Confirm that transactions were forwarded to and processed by the leader.
    cluster_tests::send_many_transactions(
        validator_info,
        &cluster.funding_keypair,
        &cluster_tests::TpuSender::new(),
        10,
        20,
    );
}

/// Validates the basic `exit_restart_node()` flow without involving snapshots or tower
/// surgery, so restart regressions are caught before the more specialized restart tests run.
#[test]
#[serial]
fn test_restart_node() {
    agave_logger::setup_with_default(RUST_LOG_FILTER);
    error!("test_restart_node");
    let slots_per_epoch = MINIMUM_SLOTS_PER_EPOCH * 2;
    let ticks_per_slot = 16;
    let validator_config = ValidatorConfig::default_for_test();
    let mut cluster = LocalCluster::new(
        &mut ClusterConfig {
            node_stakes: vec![DEFAULT_NODE_STAKE],
            validator_configs: vec![safe_clone_config(&validator_config)],
            ticks_per_slot,
            slots_per_epoch,
            stakers_slot_offset: slots_per_epoch,
            skip_warmup_slots: true,
            ..ClusterConfig::default()
        },
        SocketAddrSpace::Unspecified,
    );
    let nodes = cluster.get_node_pubkeys();
    cluster_tests::sleep_n_epochs(
        1.0,
        &cluster.genesis_config.poh_config,
        ticks_per_slot,
        slots_per_epoch,
    );
    cluster.exit_restart_node(&nodes[0], validator_config, SocketAddrSpace::Unspecified);
    cluster_tests::sleep_n_epochs(
        0.5,
        &cluster.genesis_config.poh_config,
        ticks_per_slot,
        slots_per_epoch,
    );
    cluster_tests::send_many_transactions(
        &cluster.entry_point_info,
        &cluster.funding_keypair,
        &cluster_tests::TpuSender::new(),
        10,
        1,
    );
}

/// Checks that a `MainnetBeta` local cluster boots with the expected builtins at epoch 0.
/// This is cluster-type initialization coverage rather than consensus or networking coverage.
#[test]
#[serial]
fn test_mainnet_beta_cluster_type() {
    agave_logger::setup_with_default(RUST_LOG_FILTER);

    let mut config = ClusterConfig {
        cluster_type: ClusterType::MainnetBeta,
        node_stakes: vec![DEFAULT_NODE_STAKE],
        validator_configs: make_identical_validator_configs(
            &ValidatorConfig::default_for_test(),
            1,
        ),
        ..ClusterConfig::default()
    };
    let cluster = LocalCluster::new(&mut config, SocketAddrSpace::Unspecified);
    let cluster_nodes = discover_validators(
        &cluster.entry_point_info.gossip().unwrap(),
        1,
        cluster.entry_point_info.shred_version(),
        SocketAddrSpace::Unspecified,
    )
    .unwrap();
    assert_eq!(cluster_nodes.len(), 1);

    let rpc_client = cluster
        .build_rpc_client(cluster.entry_point_info.pubkey())
        .unwrap();

    // Programs that are available at epoch 0
    for program_id in [
        &solana_sdk_ids::system_program::id(),
        &stake::program::id(),
        &solana_vote_program::id(),
        &solana_sdk_ids::bpf_loader_deprecated::id(),
        &solana_sdk_ids::bpf_loader::id(),
        &solana_sdk_ids::bpf_loader_upgradeable::id(),
    ]
    .iter()
    {
        assert_matches!(
            (
                program_id,
                rpc_client
                    .get_account_with_commitment(program_id, CommitmentConfig::processed())
                    .unwrap()
                    .value
            ),
            (_program_id, Some(_))
        );
    }

    // Programs that are not available at epoch 0
    for program_id in [].iter() {
        assert_eq!(
            (
                program_id,
                rpc_client
                    .get_account_with_commitment(program_id, CommitmentConfig::processed())
                    .unwrap()
                    .value
            ),
            (program_id, None)
        );
    }
}

/// Verifies full-snapshot download from a running validator and bootstrap of a new node
/// from that archive, without relying on incremental snapshots.
#[test]
#[serial]
fn test_snapshot_download() {
    agave_logger::setup_with_default(RUST_LOG_FILTER);
    // First set up the cluster with 1 node
    let snapshot_interval_slots = NonZeroU64::new(50).unwrap();
    let num_account_paths = 3;

    let leader_snapshot_test_config =
        setup_snapshot_validator_config(snapshot_interval_slots, num_account_paths);
    let validator_snapshot_test_config =
        setup_snapshot_validator_config(snapshot_interval_slots, num_account_paths);

    let stake = DEFAULT_NODE_STAKE;
    let mut config = ClusterConfig {
        node_stakes: vec![stake],
        validator_configs: make_identical_validator_configs(
            &leader_snapshot_test_config.validator_config,
            1,
        ),
        ..ClusterConfig::default()
    };

    let mut cluster = LocalCluster::new(&mut config, SocketAddrSpace::Unspecified);

    let full_snapshot_archives_dir = &leader_snapshot_test_config
        .validator_config
        .snapshot_config
        .full_snapshot_archives_dir;

    trace!("Waiting for snapshot");
    let full_snapshot_archive_info = cluster.wait_for_next_full_snapshot(
        full_snapshot_archives_dir,
        Some(Duration::from_secs(5 * 60)),
    );
    trace!("found: {}", full_snapshot_archive_info.path().display());

    // Download the snapshot, then boot a validator from it.
    download_snapshot_archive(
        &cluster.entry_point_info.rpc().unwrap(),
        &validator_snapshot_test_config
            .validator_config
            .snapshot_config
            .full_snapshot_archives_dir,
        &validator_snapshot_test_config
            .validator_config
            .snapshot_config
            .incremental_snapshot_archives_dir,
        (
            full_snapshot_archive_info.slot(),
            *full_snapshot_archive_info.hash(),
        ),
        SnapshotArchiveKind::Full,
        validator_snapshot_test_config
            .validator_config
            .snapshot_config
            .maximum_full_snapshot_archives_to_retain,
        validator_snapshot_test_config
            .validator_config
            .snapshot_config
            .maximum_incremental_snapshot_archives_to_retain,
        false,
        &mut None,
    )
    .unwrap();

    cluster.add_validator(
        &validator_snapshot_test_config.validator_config,
        stake,
        &ValidatorKeys::new(),
        true, // should_setup_accounts
        SocketAddrSpace::Unspecified,
    );
}

/// Extends full-snapshot bootstrap coverage to the combined full-plus-incremental
/// download path, which exercises the incremental archive selection and restore flow.
#[test]
#[serial]
fn test_incremental_snapshot_download() {
    agave_logger::setup_with_default(RUST_LOG_FILTER);
    // First set up the cluster with 1 node
    let incremental_snapshot_interval = 9;
    let full_snapshot_interval = incremental_snapshot_interval * 3;
    let num_account_paths = 3;

    let leader_snapshot_test_config = SnapshotValidatorConfig::new(
        SnapshotInterval::Slots(NonZeroU64::new(full_snapshot_interval).unwrap()),
        SnapshotInterval::Slots(NonZeroU64::new(incremental_snapshot_interval).unwrap()),
        num_account_paths,
    );
    let validator_snapshot_test_config = SnapshotValidatorConfig::new(
        SnapshotInterval::Slots(NonZeroU64::new(full_snapshot_interval).unwrap()),
        SnapshotInterval::Slots(NonZeroU64::new(incremental_snapshot_interval).unwrap()),
        num_account_paths,
    );

    let stake = DEFAULT_NODE_STAKE;
    let mut config = ClusterConfig {
        node_stakes: vec![stake],
        validator_configs: make_identical_validator_configs(
            &leader_snapshot_test_config.validator_config,
            1,
        ),
        ..ClusterConfig::default()
    };

    let mut cluster = LocalCluster::new(&mut config, SocketAddrSpace::Unspecified);

    let full_snapshot_archives_dir = &leader_snapshot_test_config
        .validator_config
        .snapshot_config
        .full_snapshot_archives_dir;
    let incremental_snapshot_archives_dir = &leader_snapshot_test_config
        .validator_config
        .snapshot_config
        .incremental_snapshot_archives_dir;

    debug!(
        "snapshot config:\n\tfull snapshot interval: {full_snapshot_interval}\n\tincremental \
         snapshot interval: {incremental_snapshot_interval}",
    );
    debug!(
        "leader config:\n\tbank snapshots dir: {}\n\tfull snapshot archives dir: \
         {}\n\tincremental snapshot archives dir: {}",
        leader_snapshot_test_config
            .bank_snapshots_dir
            .path()
            .display(),
        leader_snapshot_test_config
            .full_snapshot_archives_dir
            .path()
            .display(),
        leader_snapshot_test_config
            .incremental_snapshot_archives_dir
            .path()
            .display(),
    );
    debug!(
        "validator config:\n\tbank snapshots dir: {}\n\tfull snapshot archives dir: \
         {}\n\tincremental snapshot archives dir: {}",
        validator_snapshot_test_config
            .bank_snapshots_dir
            .path()
            .display(),
        validator_snapshot_test_config
            .full_snapshot_archives_dir
            .path()
            .display(),
        validator_snapshot_test_config
            .incremental_snapshot_archives_dir
            .path()
            .display(),
    );

    trace!("Waiting for snapshots");
    let (incremental_snapshot_archive_info, full_snapshot_archive_info) = cluster
        .wait_for_next_incremental_snapshot(
            full_snapshot_archives_dir,
            incremental_snapshot_archives_dir,
            Some(Duration::from_secs(5 * 60)),
        );
    trace!(
        "found: {} and {}",
        full_snapshot_archive_info.path().display(),
        incremental_snapshot_archive_info.path().display()
    );

    // Download the snapshots, then boot a validator from them.
    download_snapshot_archive(
        &cluster.entry_point_info.rpc().unwrap(),
        &validator_snapshot_test_config
            .validator_config
            .snapshot_config
            .full_snapshot_archives_dir,
        &validator_snapshot_test_config
            .validator_config
            .snapshot_config
            .incremental_snapshot_archives_dir,
        (
            full_snapshot_archive_info.slot(),
            *full_snapshot_archive_info.hash(),
        ),
        SnapshotArchiveKind::Full,
        validator_snapshot_test_config
            .validator_config
            .snapshot_config
            .maximum_full_snapshot_archives_to_retain,
        validator_snapshot_test_config
            .validator_config
            .snapshot_config
            .maximum_incremental_snapshot_archives_to_retain,
        false,
        &mut None,
    )
    .unwrap();

    download_snapshot_archive(
        &cluster.entry_point_info.rpc().unwrap(),
        &validator_snapshot_test_config
            .validator_config
            .snapshot_config
            .full_snapshot_archives_dir,
        &validator_snapshot_test_config
            .validator_config
            .snapshot_config
            .incremental_snapshot_archives_dir,
        (
            incremental_snapshot_archive_info.slot(),
            *incremental_snapshot_archive_info.hash(),
        ),
        SnapshotArchiveKind::Incremental(incremental_snapshot_archive_info.base_slot()),
        validator_snapshot_test_config
            .validator_config
            .snapshot_config
            .maximum_full_snapshot_archives_to_retain,
        validator_snapshot_test_config
            .validator_config
            .snapshot_config
            .maximum_incremental_snapshot_archives_to_retain,
        false,
        &mut None,
    )
    .unwrap();

    cluster.add_validator(
        &validator_snapshot_test_config.validator_config,
        stake,
        &ValidatorKeys::new(),
        true, // should_setup_accounts
        SocketAddrSpace::Unspecified,
    );
}

/// Test the scenario where a node starts up from a snapshot and its blockstore has enough new
/// roots that cross the full snapshot interval.  In this scenario, the node needs to take a full
/// snapshot while processing the blockstore so that once the background services start up, there
/// is the correct full snapshot available to take subsequent incremental snapshots.
///
/// For this test...
/// - Start a leader node and run it long enough to take a full and incremental snapshot
/// - Download those snapshots to a validator node
/// - Copy the validator snapshots to a back up directory
/// - Start up the validator node
/// - Wait for the validator node to see enough root slots to cross the full snapshot interval
/// - Delete the snapshots on the validator node and restore the ones from the backup
/// - Restart the validator node to trigger the scenario we're trying to test
/// - Wait for the validator node to generate a new incremental snapshot
/// - Copy the new incremental snapshot (and its associated full snapshot) to another new validator
/// - Start up this new validator to ensure the snapshots from ^^^ are good
#[test]
#[serial]
fn test_incremental_snapshot_download_with_crossing_full_snapshot_interval_at_startup() {
    agave_logger::setup_with_default(RUST_LOG_FILTER);
    // If these intervals change, also make sure to change the loop timers accordingly.
    let incremental_snapshot_interval = 9;
    let full_snapshot_interval = incremental_snapshot_interval * 5;

    let num_account_paths = 3;
    let leader_snapshot_test_config = SnapshotValidatorConfig::new(
        SnapshotInterval::Slots(NonZeroU64::new(full_snapshot_interval).unwrap()),
        SnapshotInterval::Slots(NonZeroU64::new(incremental_snapshot_interval).unwrap()),
        num_account_paths,
    );
    let mut validator_snapshot_test_config = SnapshotValidatorConfig::new(
        SnapshotInterval::Slots(NonZeroU64::new(full_snapshot_interval).unwrap()),
        SnapshotInterval::Slots(NonZeroU64::new(incremental_snapshot_interval).unwrap()),
        num_account_paths,
    );
    // The test has asserts that require the validator always boots from snapshot archives
    validator_snapshot_test_config
        .validator_config
        .use_snapshot_archives_at_startup = UseSnapshotArchivesAtStartup::Always;
    let stake = DEFAULT_NODE_STAKE;
    let mut config = ClusterConfig {
        node_stakes: vec![stake],
        validator_configs: make_identical_validator_configs(
            &leader_snapshot_test_config.validator_config,
            1,
        ),
        ..ClusterConfig::default()
    };

    let mut cluster = LocalCluster::new(&mut config, SocketAddrSpace::Unspecified);

    info!(
        "snapshot config:\n\tfull snapshot interval: {full_snapshot_interval:?}\n\tincremental \
         snapshot interval: {incremental_snapshot_interval:?}",
    );
    debug!(
        "leader config:\n\tbank snapshots dir: {}\n\tfull snapshot archives dir: \
         {}\n\tincremental snapshot archives dir: {}",
        leader_snapshot_test_config
            .bank_snapshots_dir
            .path()
            .display(),
        leader_snapshot_test_config
            .full_snapshot_archives_dir
            .path()
            .display(),
        leader_snapshot_test_config
            .incremental_snapshot_archives_dir
            .path()
            .display(),
    );
    debug!(
        "validator config:\n\tbank snapshots dir: {}\n\tfull snapshot archives dir: \
         {}\n\tincremental snapshot archives dir: {}",
        validator_snapshot_test_config
            .bank_snapshots_dir
            .path()
            .display(),
        validator_snapshot_test_config
            .full_snapshot_archives_dir
            .path()
            .display(),
        validator_snapshot_test_config
            .incremental_snapshot_archives_dir
            .path()
            .display(),
    );

    info!("Waiting for leader to create the next incremental snapshot...");
    let (incremental_snapshot_archive, full_snapshot_archive) =
        LocalCluster::wait_for_next_incremental_snapshot(
            &cluster,
            leader_snapshot_test_config
                .full_snapshot_archives_dir
                .path(),
            leader_snapshot_test_config
                .incremental_snapshot_archives_dir
                .path(),
            Some(Duration::from_secs(5 * 60)),
        );
    info!(
        "Found snapshots:\n\tfull snapshot: {}\n\tincremental snapshot: {}",
        full_snapshot_archive.path().display(),
        incremental_snapshot_archive.path().display()
    );
    assert_eq!(
        full_snapshot_archive.slot(),
        incremental_snapshot_archive.base_slot()
    );
    info!("Waiting for leader to create snapshots... DONE");

    // Download the snapshots, then boot a validator from them.
    info!("Downloading full snapshot to validator...");
    download_snapshot_archive(
        &cluster.entry_point_info.rpc().unwrap(),
        validator_snapshot_test_config
            .full_snapshot_archives_dir
            .path(),
        validator_snapshot_test_config
            .incremental_snapshot_archives_dir
            .path(),
        (full_snapshot_archive.slot(), *full_snapshot_archive.hash()),
        SnapshotArchiveKind::Full,
        validator_snapshot_test_config
            .validator_config
            .snapshot_config
            .maximum_full_snapshot_archives_to_retain,
        validator_snapshot_test_config
            .validator_config
            .snapshot_config
            .maximum_incremental_snapshot_archives_to_retain,
        false,
        &mut None,
    )
    .unwrap();
    let downloaded_full_snapshot_archive = snapshot_paths::get_highest_full_snapshot_archive_info(
        validator_snapshot_test_config
            .full_snapshot_archives_dir
            .path(),
    )
    .unwrap();
    info!(
        "Downloaded full snapshot, slot: {}",
        downloaded_full_snapshot_archive.slot()
    );

    info!("Downloading incremental snapshot to validator...");
    download_snapshot_archive(
        &cluster.entry_point_info.rpc().unwrap(),
        validator_snapshot_test_config
            .full_snapshot_archives_dir
            .path(),
        validator_snapshot_test_config
            .incremental_snapshot_archives_dir
            .path(),
        (
            incremental_snapshot_archive.slot(),
            *incremental_snapshot_archive.hash(),
        ),
        SnapshotArchiveKind::Incremental(incremental_snapshot_archive.base_slot()),
        validator_snapshot_test_config
            .validator_config
            .snapshot_config
            .maximum_full_snapshot_archives_to_retain,
        validator_snapshot_test_config
            .validator_config
            .snapshot_config
            .maximum_incremental_snapshot_archives_to_retain,
        false,
        &mut None,
    )
    .unwrap();
    let downloaded_incremental_snapshot_archive =
        snapshot_paths::get_highest_incremental_snapshot_archive_info(
            validator_snapshot_test_config
                .incremental_snapshot_archives_dir
                .path(),
            full_snapshot_archive.slot(),
        )
        .unwrap();
    info!(
        "Downloaded incremental snapshot, slot: {}, base slot: {}",
        downloaded_incremental_snapshot_archive.slot(),
        downloaded_incremental_snapshot_archive.base_slot(),
    );
    assert_eq!(
        downloaded_full_snapshot_archive.slot(),
        downloaded_incremental_snapshot_archive.base_slot()
    );

    // closure to copy files in a directory to another directory
    let copy_files = |from: &Path, to: &Path| {
        trace!(
            "copying files from dir {}, to dir {}",
            from.display(),
            to.display()
        );
        for entry in fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                continue;
            }
            let from_file_path = entry.path();
            let to_file_path = to.join(from_file_path.file_name().unwrap());
            trace!(
                "\t\tcopying file from {} to {}...",
                from_file_path.display(),
                to_file_path.display()
            );
            fs::copy(from_file_path, to_file_path).unwrap();
        }
    };
    // closure to delete files in a directory
    let delete_files = |dir: &Path| {
        trace!("deleting files in dir {}", dir.display());
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                continue;
            }
            let file_path = entry.path();
            trace!("\t\tdeleting file {}...", file_path.display());
            fs::remove_file(file_path).unwrap();
        }
    };

    let copy_files_with_remote = |from: &Path, to: &Path| {
        copy_files(from, to);
        let remote_from = snapshot_paths::build_snapshot_archives_remote_dir(from);
        let remote_to = snapshot_paths::build_snapshot_archives_remote_dir(to);
        let _ = fs::create_dir_all(&remote_from);
        let _ = fs::create_dir_all(&remote_to);
        copy_files(&remote_from, &remote_to);
    };

    let delete_files_with_remote = |from: &Path| {
        delete_files(from);
        let remote_dir = snapshot_paths::build_snapshot_archives_remote_dir(from);
        let _ = fs::create_dir_all(&remote_dir);
        delete_files(&remote_dir);
    };

    // After downloading the snapshots, copy them over to a backup directory.  Later we'll need to
    // restart the node and guarantee that the only snapshots present are these initial ones.  So,
    // the easiest way to do that is create a backup now, delete the ones on the node before
    // restart, then copy the backup ones over again.
    let backup_validator_full_snapshot_archives_dir = tempfile::tempdir_in(farf_dir()).unwrap();
    trace!(
        "Backing up validator full snapshots to dir: {}...",
        backup_validator_full_snapshot_archives_dir.path().display()
    );
    copy_files_with_remote(
        validator_snapshot_test_config
            .full_snapshot_archives_dir
            .path(),
        backup_validator_full_snapshot_archives_dir.path(),
    );
    let backup_validator_incremental_snapshot_archives_dir =
        tempfile::tempdir_in(farf_dir()).unwrap();
    trace!(
        "Backing up validator incremental snapshots to dir: {}...",
        backup_validator_incremental_snapshot_archives_dir
            .path()
            .display()
    );
    copy_files_with_remote(
        validator_snapshot_test_config
            .incremental_snapshot_archives_dir
            .path(),
        backup_validator_incremental_snapshot_archives_dir.path(),
    );

    info!("Starting the validator...");
    let validator_keys = ValidatorKeys::new();
    let validator_identity = validator_keys.node_keypair.clone();
    cluster.add_validator(
        &validator_snapshot_test_config.validator_config,
        stake,
        &validator_keys,
        true,
        SocketAddrSpace::Unspecified,
    );
    info!("Starting the validator... DONE");

    // To ensure that a snapshot will be taken during startup, the blockstore needs to have roots
    // that cross a full snapshot interval.
    let starting_slot = incremental_snapshot_archive.slot();
    let next_full_snapshot_slot = starting_slot + full_snapshot_interval;
    info!(
        "Waiting for the validator to see enough slots to cross a full snapshot interval \
         ({next_full_snapshot_slot})..."
    );
    let timer = Instant::now();
    loop {
        let validator_current_slot = cluster
            .build_rpc_client(&validator_identity.pubkey())
            .unwrap()
            .get_slot_with_commitment(CommitmentConfig::finalized())
            .unwrap();
        trace!("validator current slot: {validator_current_slot}");
        if validator_current_slot > next_full_snapshot_slot {
            break;
        }
        assert!(
            timer.elapsed() < Duration::from_secs(30),
            "It should not take longer than 30 seconds to cross the next full snapshot interval."
        );
        std::thread::yield_now();
    }
    info!(
        "Waited {:?} for the validator to see enough slots to cross a full snapshot interval... \
         DONE",
        timer.elapsed()
    );

    // Get the highest full snapshot archive info for the validator, now that it has crossed the
    // next full snapshot interval.  We are going to use this to look up the same snapshot on the
    // leader, which we'll then use to compare to the full snapshot the validator will create
    // during startup.  This ensures the snapshot creation process during startup is correct.
    //
    // Putting this all in its own block so its clear we're only intended to keep the leader's info
    let leader_full_snapshot_archive_for_comparison = {
        let validator_full_snapshot = snapshot_paths::get_highest_full_snapshot_archive_info(
            validator_snapshot_test_config
                .full_snapshot_archives_dir
                .path(),
        )
        .unwrap();

        // Now get the same full snapshot on the LEADER that we just got from the validator
        let mut leader_full_snapshots = snapshot_paths::full_snapshot_archives_iter(
            leader_snapshot_test_config
                .full_snapshot_archives_dir
                .path(),
        )
        .collect::<Vec<_>>();
        leader_full_snapshots.retain(|full_snapshot| {
            full_snapshot.slot() == validator_full_snapshot.slot()
                && full_snapshot.hash() == validator_full_snapshot.hash()
        });
        let leader_full_snapshot = leader_full_snapshots.first().unwrap();

        // And for sanity, the full snapshot from the leader and the validator MUST be the same
        assert_eq!(
            (
                validator_full_snapshot.slot(),
                validator_full_snapshot.hash()
            ),
            (leader_full_snapshot.slot(), leader_full_snapshot.hash())
        );

        leader_full_snapshot.clone()
    };
    info!(
        "leader full snapshot archive for comparison: \
         {leader_full_snapshot_archive_for_comparison:#?}"
    );

    // Stop the validator before we reset its snapshots
    info!("Stopping the validator...");
    let validator_info = cluster.exit_node(&validator_identity.pubkey());
    info!("Stopping the validator... DONE");

    info!("Delete all the snapshots on the validator and restore the originals from the backup...");
    delete_files_with_remote(
        validator_snapshot_test_config
            .full_snapshot_archives_dir
            .path(),
    );
    delete_files_with_remote(
        validator_snapshot_test_config
            .incremental_snapshot_archives_dir
            .path(),
    );
    copy_files_with_remote(
        backup_validator_full_snapshot_archives_dir.path(),
        validator_snapshot_test_config
            .full_snapshot_archives_dir
            .path(),
    );
    copy_files_with_remote(
        backup_validator_incremental_snapshot_archives_dir.path(),
        validator_snapshot_test_config
            .incremental_snapshot_archives_dir
            .path(),
    );
    info!(
        "Delete all the snapshots on the validator and restore the originals from the backup... \
         DONE"
    );

    // Get the highest full snapshot slot *before* restarting, as a comparison
    let validator_full_snapshot_slot_at_startup =
        snapshot_paths::get_highest_full_snapshot_archive_slot(
            validator_snapshot_test_config
                .full_snapshot_archives_dir
                .path(),
        )
        .unwrap();

    info!(
        "Restarting the validator with full snapshot {validator_full_snapshot_slot_at_startup}..."
    );
    cluster.restart_node(
        &validator_identity.pubkey(),
        validator_info,
        SocketAddrSpace::Unspecified,
    );
    info!("Restarting the validator... DONE");

    // Now, we want to ensure that the validator can make a new incremental snapshot based on the
    // new full snapshot that was created during the restart.
    info!("Waiting for the validator to make new snapshots...");
    let validator_next_full_snapshot_slot =
        validator_full_snapshot_slot_at_startup + full_snapshot_interval;
    let validator_next_incremental_snapshot_slot =
        validator_next_full_snapshot_slot + incremental_snapshot_interval;
    info!("Waiting for validator next full snapshot slot: {validator_next_full_snapshot_slot}");
    info!(
        "Waiting for validator next incremental snapshot slot: \
         {validator_next_incremental_snapshot_slot}"
    );
    let timer = Instant::now();
    loop {
        if let Some(full_snapshot_slot) = snapshot_paths::get_highest_full_snapshot_archive_slot(
            validator_snapshot_test_config
                .full_snapshot_archives_dir
                .path(),
        ) && full_snapshot_slot >= validator_next_full_snapshot_slot
            && let Some(incremental_snapshot_slot) =
                snapshot_paths::get_highest_incremental_snapshot_archive_slot(
                    validator_snapshot_test_config
                        .incremental_snapshot_archives_dir
                        .path(),
                    full_snapshot_slot,
                )
            && incremental_snapshot_slot >= validator_next_incremental_snapshot_slot
        {
            // specific incremental snapshot is not important, just that one was created
            info!(
                "Validator made new snapshots, full snapshot slot: {full_snapshot_slot}, \
                 incremental snapshot slot: {incremental_snapshot_slot}",
            );
            break;
        }

        assert!(
            timer.elapsed() < Duration::from_secs(30),
            "It should not take longer than 30 seconds to cross the next incremental snapshot \
             interval."
        );
        std::thread::yield_now();
    }
    info!(
        "Waited {:?} for the validator to make new snapshots... DONE",
        timer.elapsed()
    );

    // Check to make sure that the full snapshot the validator created during startup is the same
    // or one greater than the snapshot the leader created.
    let validator_full_snapshot_archives = snapshot_paths::full_snapshot_archives_iter(
        validator_snapshot_test_config
            .full_snapshot_archives_dir
            .path(),
    )
    .collect::<Vec<_>>();
    info!("validator full snapshot archives: {validator_full_snapshot_archives:#?}");
    let validator_full_snapshot_archive_for_comparison = validator_full_snapshot_archives
        .into_iter()
        .find(|validator_full_snapshot_archive| {
            validator_full_snapshot_archive.slot()
                == leader_full_snapshot_archive_for_comparison.slot()
        })
        .expect("validator created an unexpected full snapshot");
    info!(
        "Validator full snapshot archive for comparison: \
         {validator_full_snapshot_archive_for_comparison:#?}"
    );
    assert_eq!(
        validator_full_snapshot_archive_for_comparison.hash(),
        leader_full_snapshot_archive_for_comparison.hash(),
    );

    // And lastly, startup another node with the new snapshots to ensure they work
    let final_validator_snapshot_test_config = SnapshotValidatorConfig::new(
        SnapshotInterval::Slots(NonZeroU64::new(full_snapshot_interval).unwrap()),
        SnapshotInterval::Slots(NonZeroU64::new(incremental_snapshot_interval).unwrap()),
        num_account_paths,
    );

    // Copy over the snapshots to the new node that it will boot from
    copy_files(
        validator_snapshot_test_config
            .full_snapshot_archives_dir
            .path(),
        final_validator_snapshot_test_config
            .full_snapshot_archives_dir
            .path(),
    );
    copy_files(
        validator_snapshot_test_config
            .incremental_snapshot_archives_dir
            .path(),
        final_validator_snapshot_test_config
            .incremental_snapshot_archives_dir
            .path(),
    );

    info!("Starting final validator...");
    cluster.add_validator(
        &final_validator_snapshot_test_config.validator_config,
        stake,
        &ValidatorKeys::new(),
        true, // should_setup_accounts
        SocketAddrSpace::Unspecified,
    );
    info!("Starting final validator... DONE");
}
/// Proves a snapshot-restored validator repairs only slots at or above the snapshot's
/// slot floor instead of backfilling older ledger history.
#[test]
#[serial]
fn test_snapshots_blockstore_floor() {
    agave_logger::setup_with_default(RUST_LOG_FILTER);
    // First set up the cluster with 1 snapshotting leader
    let snapshot_interval_slots = NonZeroU64::new(100).unwrap();
    let num_account_paths = 4;

    let leader_snapshot_test_config =
        setup_snapshot_validator_config(snapshot_interval_slots, num_account_paths);
    let mut validator_snapshot_test_config =
        setup_snapshot_validator_config(snapshot_interval_slots, num_account_paths);

    let full_snapshot_archives_dir = &leader_snapshot_test_config
        .validator_config
        .snapshot_config
        .full_snapshot_archives_dir;

    let mut config = ClusterConfig {
        node_stakes: vec![DEFAULT_NODE_STAKE],
        validator_configs: make_identical_validator_configs(
            &leader_snapshot_test_config.validator_config,
            1,
        ),
        ..ClusterConfig::default()
    };

    let mut cluster = LocalCluster::new(&mut config, SocketAddrSpace::Unspecified);

    trace!("Waiting for snapshot tar to be generated with slot",);

    let archive_info = loop {
        let archive =
            snapshot_paths::get_highest_full_snapshot_archive_info(full_snapshot_archives_dir);
        if archive.is_some() {
            trace!("snapshot exists");
            break archive.unwrap();
        }
        sleep(Duration::from_millis(5000));
    };

    // Copy archive to validator's snapshot output directory
    let validator_archive_path = snapshot_paths::build_full_snapshot_archive_path(
        validator_snapshot_test_config
            .full_snapshot_archives_dir
            .keep(),
        archive_info.slot(),
        archive_info.hash(),
        validator_snapshot_test_config
            .validator_config
            .snapshot_config
            .archive_format,
    );
    fs::hard_link(archive_info.path(), validator_archive_path).unwrap();
    let slot_floor = archive_info.slot();

    // Start up a new node from a snapshot
    let cluster_nodes = discover_validators(
        &cluster.entry_point_info.gossip().unwrap(),
        1,
        cluster.entry_point_info.shred_version(),
        SocketAddrSpace::Unspecified,
    )
    .unwrap();
    let mut known_validators = HashSet::new();
    known_validators.insert(*cluster_nodes[0].pubkey());
    validator_snapshot_test_config
        .validator_config
        .known_validators = Some(known_validators);

    cluster.add_validator(
        &validator_snapshot_test_config.validator_config,
        DEFAULT_NODE_STAKE,
        &ValidatorKeys::new(),
        true, // should_setup_accounts
        SocketAddrSpace::Unspecified,
    );
    let all_pubkeys = cluster.get_node_pubkeys();
    let validator_id = all_pubkeys
        .into_iter()
        .find(|x| x != cluster.entry_point_info.pubkey())
        .unwrap();
    let rpc_client = cluster.build_rpc_client(&validator_id).unwrap();
    let mut current_slot = 0;

    // Let this validator run a while with repair
    let target_slot = slot_floor + 40;
    while current_slot <= target_slot {
        trace!("current_slot: {current_slot}");
        if let Ok(slot) = rpc_client.get_slot_with_commitment(CommitmentConfig::processed()) {
            current_slot = slot;
        } else {
            continue;
        }
        sleep(Duration::from_secs(1));
    }

    // Check the validator ledger doesn't contain any slots < slot_floor
    cluster.close_preserve_ledgers();
    let validator_ledger_path = &cluster.validators[&validator_id];
    let blockstore = Blockstore::open(&validator_ledger_path.info.ledger_path).unwrap();

    // Skip the zeroth slot in blockstore that the ledger is initialized with
    let (first_slot, _) = blockstore.slot_meta_iterator(1).unwrap().next().unwrap();

    assert_eq!(first_slot, slot_floor);
}

/// Repeats snapshot-based restart several times while preserving balances and continuing
/// to generate valid snapshots, which catches restart idempotence issues.
#[test]
#[serial]
fn test_snapshots_restart_validity() {
    agave_logger::setup_with_default(RUST_LOG_FILTER);
    let snapshot_interval_slots = NonZeroU64::new(100).unwrap();
    let num_account_paths = 1;
    let mut snapshot_test_config =
        setup_snapshot_validator_config(snapshot_interval_slots, num_account_paths);
    let full_snapshot_archives_dir = &snapshot_test_config
        .validator_config
        .snapshot_config
        .full_snapshot_archives_dir;

    // Set up the cluster with 1 snapshotting validator
    let mut all_account_storage_dirs = vec![std::mem::take(
        &mut snapshot_test_config.account_storage_dirs,
    )];
    let mut config = ClusterConfig {
        node_stakes: vec![DEFAULT_NODE_STAKE],
        validator_configs: make_identical_validator_configs(
            &snapshot_test_config.validator_config,
            1,
        ),
        ..ClusterConfig::default()
    };

    // Create and reboot the node from snapshot `num_runs` times
    let num_runs = 3;
    let mut expected_balances = HashMap::new();
    let mut cluster = LocalCluster::new(&mut config, SocketAddrSpace::Unspecified);
    let tpu_sender = cluster_tests::TpuSender::new();
    for i in 1..num_runs {
        info!("run {i}");
        // Push transactions to one of the nodes and confirm that transactions were
        // forwarded to and processed.
        trace!("Sending transactions");
        let new_balances = cluster_tests::send_many_transactions(
            &cluster.entry_point_info,
            &cluster.funding_keypair,
            &tpu_sender,
            10,
            10,
        );

        expected_balances.extend(new_balances);

        cluster.wait_for_next_full_snapshot(
            full_snapshot_archives_dir,
            Some(Duration::from_secs(5 * 60)),
        );

        // Create new account paths since validator exit is not guaranteed to cleanup RPC threads,
        // which may delete the old accounts on exit at any point
        let (new_account_storage_dirs, new_account_storage_paths) =
            generate_account_paths(num_account_paths);
        all_account_storage_dirs.push(new_account_storage_dirs);
        snapshot_test_config.validator_config.account_paths = new_account_storage_paths;

        // Restart node
        trace!("Restarting cluster from snapshot");
        let nodes = cluster.get_node_pubkeys();
        cluster.exit_restart_node(
            &nodes[0],
            safe_clone_config(&snapshot_test_config.validator_config),
            SocketAddrSpace::Unspecified,
        );

        // Verify account balances on validator
        trace!("Verifying balances");
        cluster_tests::verify_balances(expected_balances.clone(), &cluster.entry_point_info);

        // Check that we can still push transactions
        trace!("Spending and verifying");
        cluster_tests::spend_and_verify_all_nodes(
            &cluster.entry_point_info,
            &cluster.funding_keypair,
            1,
            HashSet::new(),
            SocketAddrSpace::Unspecified,
            &tpu_sender,
        );
    }
}
/// Checks that concurrent program-account scans stay consistent at rooted/finalized
/// commitment while transfers keep mutating accounts in the background.
#[test]
#[serial]
fn test_run_test_load_program_accounts_root() {
    run_test_load_program_accounts(CommitmentConfig::finalized());
}
/// Reuses the program-account load scanner under a partition/recovery cycle, so the
/// rooted-account consistency check is exercised under repair as well as steady state.
#[test]
#[serial]
fn test_run_test_load_program_accounts_partition_root() {
    let num_slots_per_validator = 8;
    let partitions: [usize; 2] = [1, 1];
    let (leader_schedule, validator_keys) = create_custom_leader_schedule_with_random_keys(&[
        num_slots_per_validator,
        num_slots_per_validator,
    ]);

    let (update_client_sender, update_client_receiver) = bounded(1024);
    let (scan_client_sender, scan_client_receiver) = bounded(1024);
    let exit = Arc::new(AtomicBool::new(false));

    let (t_update, t_scan, additional_accounts) = setup_transfer_scan_threads(
        100,
        exit.clone(),
        CommitmentConfig::finalized(),
        update_client_receiver,
        scan_client_receiver,
    );

    let on_partition_start = |cluster: &mut LocalCluster, _: &mut ()| {
        let rpc_client = cluster
            .build_rpc_client(cluster.entry_point_info.pubkey())
            .unwrap();
        let leader_pubkey = rpc_client
            .get_slot_leader()
            .expect("on_partition_start: get_slot_leader");
        let tpu_addr = rpc_client
            .get_cluster_nodes()
            .expect("on_partition_start: get_cluster_nodes")
            .into_iter()
            .find(|n| n.pubkey == leader_pubkey.to_string())
            .and_then(|n| n.tpu_quic)
            .or_else(|| {
                cluster
                    .get_contact_info(cluster.entry_point_info.pubkey())
                    .unwrap()
                    .tpu(Protocol::QUIC)
            })
            .expect("on_partition_start: leader has no QUIC TPU address");
        update_client_sender.send((rpc_client, tpu_addr)).unwrap();
        let scan_rpc_client = cluster
            .build_rpc_client(cluster.entry_point_info.pubkey())
            .unwrap();
        scan_client_sender.send(scan_rpc_client).unwrap();
    };

    let on_partition_before_resolved = |_: &mut LocalCluster, _: &mut ()| {};

    let on_partition_resolved = |cluster: &mut LocalCluster, _: &mut ()| {
        cluster.check_for_new_roots(
            20,
            "run_test_load_program_accounts_partition",
            SocketAddrSpace::Unspecified,
        );
        exit.store(true, Ordering::Relaxed);
        t_update.join().unwrap();
        t_scan.join().unwrap();
    };

    run_cluster_partition(
        &partitions,
        Some((leader_schedule, validator_keys)),
        (),
        on_partition_start,
        on_partition_before_resolved,
        on_partition_resolved,
        None,
        false,
        additional_accounts,
    );
}
fn setup_transfer_scan_threads(
    num_starting_accounts: usize,
    exit: Arc<AtomicBool>,
    scan_commitment: CommitmentConfig,
    update_client_receiver: Receiver<(Arc<RpcClient>, SocketAddr)>,
    scan_client_receiver: Receiver<Arc<RpcClient>>,
) -> (
    JoinHandle<()>,
    JoinHandle<()>,
    Vec<(Pubkey, AccountSharedData)>,
) {
    let exit_ = exit.clone();
    let starting_keypairs: Arc<Vec<Keypair>> = Arc::new(
        iter::repeat_with(Keypair::new)
            .take(num_starting_accounts)
            .collect(),
    );
    let target_keypairs: Arc<Vec<Keypair>> = Arc::new(
        iter::repeat_with(Keypair::new)
            .take(num_starting_accounts)
            .collect(),
    );
    let starting_accounts: Vec<(Pubkey, AccountSharedData)> = starting_keypairs
        .iter()
        .map(|k| {
            (
                k.pubkey(),
                AccountSharedData::new(1, 0, &system_program::id()),
            )
        })
        .collect();

    let starting_keypairs_ = starting_keypairs.clone();
    let target_keypairs_ = target_keypairs.clone();
    let t_update = Builder::new()
        .name("update".to_string())
        .spawn(move || {
            let (rpc_client, tpu_addr) = update_client_receiver.recv().unwrap();
            let tpu_sender = cluster_tests::TpuSender::new();
            let tpu_sender_inner = tpu_sender.clone();
            tpu_sender.with_connection(tpu_addr, move |sender| {
                loop {
                    if exit_.load(Ordering::Relaxed) {
                        return;
                    }
                    let (blockhash, _) = rpc_client
                        .get_latest_blockhash_with_commitment(CommitmentConfig::processed())
                        .unwrap();
                    for i in 0..starting_keypairs_.len() {
                        let tx = system_transaction::transfer(
                            &starting_keypairs_[i],
                            &target_keypairs_[i].pubkey(),
                            1,
                            blockhash,
                        );
                        tpu_sender_inner.try_send_wire_transaction(
                            sender,
                            wincode::serialize(&tx)
                                .expect("setup_transfer_scan_threads: serialize"),
                        );
                    }
                    for i in 0..starting_keypairs_.len() {
                        let tx = system_transaction::transfer(
                            &target_keypairs_[i],
                            &starting_keypairs_[i].pubkey(),
                            1,
                            blockhash,
                        );
                        tpu_sender_inner.try_send_wire_transaction(
                            sender,
                            wincode::serialize(&tx)
                                .expect("setup_transfer_scan_threads: serialize"),
                        );
                    }
                }
            });
        })
        .unwrap();

    // Scan, the total funds should add up to the original
    let mut scan_commitment_config = RpcProgramAccountsConfig::default();
    scan_commitment_config.account_config.commitment = Some(scan_commitment);
    let tracked_pubkeys: HashSet<Pubkey> = starting_keypairs
        .iter()
        .chain(target_keypairs.iter())
        .map(|k| k.pubkey())
        .collect();
    let expected_total_balance = num_starting_accounts as u64;
    let t_scan = Builder::new()
        .name("scan".to_string())
        .spawn(move || {
            let rpc_client = scan_client_receiver.recv().unwrap();
            loop {
                if exit.load(Ordering::Relaxed) {
                    return;
                }
                if let Some(total_scan_balance) = rpc_client
                    .get_program_ui_accounts_with_config(
                        &system_program::id(),
                        scan_commitment_config.clone(),
                    )
                    .ok()
                    .map(|result| {
                        result
                            .into_iter()
                            .map(|(key, account)| {
                                if tracked_pubkeys.contains(&key) {
                                    account.lamports
                                } else {
                                    0
                                }
                            })
                            .sum::<u64>()
                    })
                {
                    assert_eq!(total_scan_balance, expected_total_balance);
                }
            }
        })
        .unwrap();

    (t_update, t_scan, starting_accounts)
}

fn run_test_load_program_accounts(scan_commitment: CommitmentConfig) {
    agave_logger::setup_with_default(RUST_LOG_FILTER);
    // First set up the cluster with 2 nodes
    let slots_per_epoch = 2048;
    let node_stakes = vec![51 * DEFAULT_NODE_STAKE, 50 * DEFAULT_NODE_STAKE];
    let validator_keys: Vec<_> = create_test_validator_keys(&[
        "4qhhXNTbKD1a5vxDDLZcHKj7ELNeiivtUBxn3wUK1F5VRsQVP89VUhfXqSfgiFB14GfuBgtrQ96n9NvWQADVkcCg",
        "3kHBzVwie5vTEaY6nFCPeFT8qDpoXzn7dCEioGRNBTnUDpvwnG85w8Wq63gVWpVTP8k2a8cgcWRjSXyUkEygpXWS",
    ]);

    let num_starting_accounts = 100;
    let exit = Arc::new(AtomicBool::new(false));
    let (update_client_sender, update_client_receiver) = bounded(1024);
    let (scan_client_sender, scan_client_receiver) = bounded(1024);

    // Setup the update/scan threads
    let (t_update, t_scan, starting_accounts) = setup_transfer_scan_threads(
        num_starting_accounts,
        exit.clone(),
        scan_commitment,
        update_client_receiver,
        scan_client_receiver,
    );

    let mut validator_config = ValidatorConfig::default_for_test();
    validator_config.wait_for_supermajority = Some(0);
    let mut config = ClusterConfig {
        mint_lamports: DEFAULT_MINT_LAMPORTS + node_stakes.iter().sum::<u64>(),
        node_stakes: node_stakes.clone(),
        validator_configs: make_identical_validator_configs(&validator_config, node_stakes.len()),
        validator_keys: Some(validator_keys),
        slots_per_epoch,
        stakers_slot_offset: slots_per_epoch,
        skip_warmup_slots: true,
        additional_accounts: starting_accounts,
        ..ClusterConfig::default()
    };
    let cluster = LocalCluster::new(&mut config, SocketAddrSpace::Unspecified);

    // Give the threads a client to use for querying the cluster
    let all_pubkeys = cluster.get_node_pubkeys();
    let other_validator_id = all_pubkeys
        .into_iter()
        .find(|x| x != cluster.entry_point_info.pubkey())
        .unwrap();
    let rpc_client = cluster
        .build_rpc_client(cluster.entry_point_info.pubkey())
        .unwrap();
    let leader_pubkey = rpc_client
        .get_slot_leader()
        .expect("run_test_load_program_accounts: get_slot_leader");
    let tpu_addr = rpc_client
        .get_cluster_nodes()
        .expect("run_test_load_program_accounts: get_cluster_nodes")
        .into_iter()
        .find(|n| n.pubkey == leader_pubkey.to_string())
        .and_then(|n| n.tpu_quic)
        .or_else(|| {
            cluster
                .get_contact_info(cluster.entry_point_info.pubkey())
                .unwrap()
                .tpu(Protocol::QUIC)
        })
        .expect("run_test_load_program_accounts: leader has no QUIC TPU address");
    update_client_sender.send((rpc_client, tpu_addr)).unwrap();
    let scan_rpc_client = cluster.build_rpc_client(&other_validator_id).unwrap();
    scan_client_sender.send(scan_rpc_client).unwrap();

    // Wait for some roots to pass
    cluster.check_for_new_roots(
        40,
        "run_test_load_program_accounts",
        SocketAddrSpace::Unspecified,
    );

    // Exit and ensure no violations of consistency were found
    exit.store(true, Ordering::Relaxed);
    t_update.join().unwrap();
    t_scan.join().unwrap();
}
#[test]
#[serial]
fn test_kill_heaviest_partition_alpenglow() {
    run_kill_heaviest_partition();
}

fn run_kill_heaviest_partition() {
    // This test:
    // 1) Spins up four partitions, the heaviest being the first with more stake
    // 2) Schedules the other validators for sufficient slots in the schedule
    // so that they will still be locked out of voting for the major partition
    // when the partition resolves
    // 3) Kills the most staked partition. Validators are locked out, but should all
    // eventually choose the major partition
    // 4) Check for recovery
    let num_slots_per_validator = 8;
    let partitions: [usize; 4] = [
        11 * DEFAULT_NODE_STAKE as usize,
        10 * DEFAULT_NODE_STAKE as usize,
        10 * DEFAULT_NODE_STAKE as usize,
        10 * DEFAULT_NODE_STAKE as usize,
    ];
    let (leader_schedule, validator_keys) = create_custom_leader_schedule_with_random_keys(&[
        num_slots_per_validator * (partitions.len() - 1),
        num_slots_per_validator,
        num_slots_per_validator,
        num_slots_per_validator,
    ]);

    let empty = |_: &mut LocalCluster, _: &mut ()| {};
    let validator_to_kill = validator_keys[0].node_keypair.pubkey();
    let on_partition_resolved = |cluster: &mut LocalCluster, _: &mut ()| {
        info!("Killing validator with id: {validator_to_kill}");
        cluster.exit_node(&validator_to_kill);
        cluster.check_for_new_roots(16, "PARTITION_TEST", SocketAddrSpace::Unspecified);
    };
    run_cluster_partition(
        &partitions,
        Some((leader_schedule, validator_keys)),
        (),
        empty,
        empty,
        on_partition_resolved,
        None,
        true,
        vec![],
    )
}
/// Verifies `num_listeners` actually spawns discoverable listener nodes in gossip.
/// This is startup plumbing coverage rather than transaction or consensus coverage.
#[test]
#[serial]
fn test_listener_startup() {
    let mut config = ClusterConfig {
        node_stakes: vec![DEFAULT_NODE_STAKE],
        num_listeners: 3,
        validator_configs: make_identical_validator_configs(
            &ValidatorConfig::default_for_test(),
            1,
        ),
        ..ClusterConfig::default()
    };
    let cluster = LocalCluster::new(&mut config, SocketAddrSpace::Unspecified);
    let cluster_nodes = discover_validators(
        &cluster.entry_point_info.gossip().unwrap(),
        4,
        cluster.entry_point_info.shred_version(),
        SocketAddrSpace::Unspecified,
    )
    .unwrap();
    assert_eq!(cluster_nodes.len(), 4);
}
/// Partition/recovery test for Alpenglow
#[test]
#[serial]
fn test_cluster_partition_1_1_1_alpenglow() {
    run_test_cluster_partition(3);
}

fn run_test_cluster_partition(num_partitions: usize) {
    let empty = |_: &mut LocalCluster, _: &mut ()| {};
    let on_partition_resolved = |cluster: &mut LocalCluster, _: &mut ()| {
        cluster.check_for_new_roots(16, "PARTITION_TEST", SocketAddrSpace::Unspecified);
    };
    let partition_sizes = vec![1; num_partitions];
    run_cluster_partition(
        &partition_sizes,
        None,
        (),
        empty,
        empty,
        on_partition_resolved,
        None,
        false,
        vec![],
    )
}

/// Proves a four-node cluster stays live after killing the bootstrap leader, which is a
/// stronger failure mode than the one-node restart smoke tests.
#[test]
#[serial]
fn test_leader_failure_4() {
    agave_logger::setup_with_default(RUST_LOG_FILTER);
    error!("test_leader_failure_4");
    // Cluster needs a supermajority to remain even after taking 1 node offline,
    // so the minimum number of nodes for this test is 4.
    let num_nodes = 4;
    let mut validator_config = ValidatorConfig::default_for_test();
    validator_config.wait_for_supermajority = Some(0);
    // Embed vote and stake account in genesis to avoid waiting for stake
    // activation and race conditions around accepting gossip votes, repairing
    // blocks, etc. before we advance through too many epochs.
    let validator_keys: Option<Vec<(ValidatorKeys, bool)>> = Some(
        (0..num_nodes)
            .map(|_| (ValidatorKeys::new(), true))
            .collect(),
    );
    // Skip the warmup slots because these short epochs can cause problems when
    // bringing multiple fresh validators online that are pre-staked in genesis.
    // The problems arise because we skip their leader slots while they're still
    // starting up, experience partitioning, and can fail to generate leader
    // schedules in time because the short epochs have the same slots per epoch
    // as the total tower height, so any skipped slots can lead to not rooting,
    // not generating leader schedule, and stalling the cluster.
    let skip_warmup_slots = true;
    let mut config = ClusterConfig {
        node_stakes: vec![DEFAULT_NODE_STAKE; num_nodes],
        validator_configs: make_identical_validator_configs(&validator_config, num_nodes),
        validator_keys,
        skip_warmup_slots,
        ..ClusterConfig::default()
    };
    let local = LocalCluster::new(&mut config, SocketAddrSpace::Unspecified);

    cluster_tests::kill_entry_and_spend_and_verify_rest(
        &local.entry_point_info,
        &local
            .validators
            .get(local.entry_point_info.pubkey())
            .unwrap()
            .config
            .validator_exit,
        &local.funding_keypair,
        &cluster_tests::TpuSender::new(),
        num_nodes,
        config.ticks_per_slot * config.poh_config.target_tick_duration.as_millis() as u64,
        SocketAddrSpace::Unspecified,
    );
}
/// Test fastboot to ensure a node can boot from local state and still produce correct snapshots
///
/// 1. Start node 1 and wait for it to take snapshots
/// 2. Start node 2 with the snapshots from (1)
/// 3. Wait for node 2 to take a bank snapshot
/// 4. Restart node 2 with the local state from (3)
/// 5. Wait for node 2 to take new snapshots
/// 6. Start node 3 with the snapshots from (5)
/// 7. Wait for node 3 to take new snapshots
/// 8. Ensure the snapshots from (7) match node's 1 and 2
#[test]
#[serial]
fn test_boot_from_local_state() {
    agave_logger::setup_with_default("error,local_cluster=info");
    const FULL_SNAPSHOT_INTERVAL: SnapshotInterval =
        SnapshotInterval::Slots(NonZeroU64::new(100).unwrap());
    const INCREMENTAL_SNAPSHOT_INTERVAL: SnapshotInterval =
        SnapshotInterval::Slots(NonZeroU64::new(10).unwrap());

    let validator1_config =
        SnapshotValidatorConfig::new(FULL_SNAPSHOT_INTERVAL, INCREMENTAL_SNAPSHOT_INTERVAL, 2);
    let validator2_config =
        SnapshotValidatorConfig::new(FULL_SNAPSHOT_INTERVAL, INCREMENTAL_SNAPSHOT_INTERVAL, 4);
    let validator3_config =
        SnapshotValidatorConfig::new(FULL_SNAPSHOT_INTERVAL, INCREMENTAL_SNAPSHOT_INTERVAL, 3);

    let mut cluster_config = ClusterConfig {
        node_stakes: vec![100 * DEFAULT_NODE_STAKE],
        validator_configs: make_identical_validator_configs(&validator1_config.validator_config, 1),
        ..ClusterConfig::default()
    };
    let mut cluster = LocalCluster::new(&mut cluster_config, SocketAddrSpace::Unspecified);

    // in order to boot from local state, need to first have snapshot archives
    info!("Waiting for validator1 to create snapshots...");
    let (incremental_snapshot_archive, full_snapshot_archive) =
        LocalCluster::wait_for_next_incremental_snapshot(
            &cluster,
            &validator1_config.full_snapshot_archives_dir,
            &validator1_config.incremental_snapshot_archives_dir,
            Some(Duration::from_secs(5 * 60)),
        );
    debug!(
        "snapshot archives:\n\tfull: {full_snapshot_archive:?}\n\tincr: \
         {incremental_snapshot_archive:?}"
    );
    info!("Waiting for validator1 to create snapshots... DONE");

    info!("Copying snapshots to validator2...");
    std::fs::copy(
        full_snapshot_archive.path(),
        validator2_config
            .full_snapshot_archives_dir
            .path()
            .join(full_snapshot_archive.path().file_name().unwrap()),
    )
    .unwrap();
    std::fs::copy(
        incremental_snapshot_archive.path(),
        validator2_config
            .incremental_snapshot_archives_dir
            .path()
            .join(incremental_snapshot_archive.path().file_name().unwrap()),
    )
    .unwrap();
    info!("Copying snapshots to validator2... DONE");

    info!("Starting validator2...");
    let validator2_keys = ValidatorKeys::new();
    let validator2_identity = validator2_keys.node_keypair.clone();
    cluster.add_validator(
        &validator2_config.validator_config,
        DEFAULT_NODE_STAKE,
        &validator2_keys,
        true,
        SocketAddrSpace::Unspecified,
    );
    info!("Starting validator2... DONE");

    // wait for a new bank snapshot to fastboot from that is newer than its snapshot archives
    info!("Waiting for validator2 to create a new bank snapshot...");
    let timer = Instant::now();
    let bank_snapshot = loop {
        if let Some(bank_snapshot) =
            snapshot_utils::get_highest_bank_snapshot(&validator2_config.bank_snapshots_dir)
            && bank_snapshot.slot > incremental_snapshot_archive.slot()
        {
            break bank_snapshot;
        }
        assert!(
            timer.elapsed() < Duration::from_secs(30),
            "It should not take longer than 30 seconds to create a new bank snapshot"
        );
        std::thread::yield_now();
    };
    debug!("bank snapshot: {bank_snapshot:?}");
    info!("Waiting for validator2 to create a new bank snapshot... DONE");

    // restart WITH fastboot
    info!("Restarting validator2 from local state...");
    let mut validator2_info = cluster.exit_node(&validator2_identity.pubkey());
    validator2_info.config.use_snapshot_archives_at_startup = UseSnapshotArchivesAtStartup::Never;
    cluster.restart_node(
        &validator2_identity.pubkey(),
        validator2_info,
        SocketAddrSpace::Unspecified,
    );
    info!("Restarting validator2 from local state... DONE");

    info!("Waiting for validator2 to create snapshots...");
    let (incremental_snapshot_archive, full_snapshot_archive) =
        LocalCluster::wait_for_next_incremental_snapshot(
            &cluster,
            &validator2_config.full_snapshot_archives_dir,
            &validator2_config.incremental_snapshot_archives_dir,
            Some(Duration::from_secs(5 * 60)),
        );
    debug!(
        "snapshot archives:\n\tfull: {full_snapshot_archive:?}\n\tincr: \
         {incremental_snapshot_archive:?}"
    );
    info!("Waiting for validator2 to create snapshots... DONE");

    info!("Copying snapshots to validator3...");
    std::fs::copy(
        full_snapshot_archive.path(),
        validator3_config
            .full_snapshot_archives_dir
            .path()
            .join(full_snapshot_archive.path().file_name().unwrap()),
    )
    .unwrap();
    std::fs::copy(
        incremental_snapshot_archive.path(),
        validator3_config
            .incremental_snapshot_archives_dir
            .path()
            .join(incremental_snapshot_archive.path().file_name().unwrap()),
    )
    .unwrap();
    info!("Copying snapshots to validator3... DONE");

    info!("Starting validator3...");
    let validator3_keys = ValidatorKeys::new();
    cluster.add_validator(
        &validator3_config.validator_config,
        DEFAULT_NODE_STAKE,
        &validator3_keys,
        true,
        SocketAddrSpace::Unspecified,
    );
    info!("Starting validator3... DONE");

    // wait for a new snapshot to ensure the validator is making roots
    info!("Waiting for validator3 to create snapshots...");
    let (incremental_snapshot_archive, full_snapshot_archive) =
        LocalCluster::wait_for_next_incremental_snapshot(
            &cluster,
            &validator3_config.full_snapshot_archives_dir,
            &validator3_config.incremental_snapshot_archives_dir,
            Some(Duration::from_secs(5 * 60)),
        );
    debug!(
        "snapshot archives:\n\tfull: {full_snapshot_archive:?}\n\tincr: \
         {incremental_snapshot_archive:?}"
    );
    info!("Waiting for validator3 to create snapshots... DONE");

    // Ensure that all validators have the correct state by comparing snapshots.
    // Since validator1 has been running the longest, if may be ahead of the others,
    // so use it as the comparison for others.
    // - wait for validator1 to take new snapshots
    // - wait for the other validators to have high enough snapshots
    // - ensure the other validators' full snapshots match validator1's
    //
    // NOTE: There's a chance validator 2 or 3 has crossed the next full snapshot past what
    // validator 1 has.  If that happens, validator 2 or 3 may have purged the snapshots needed
    // to compare with validator 1, and thus assert.  If that happens, the full snapshot interval
    // may need to be adjusted larger.

    info!("Waiting for validator1 to create snapshots...");
    let (incremental_snapshot_archive, full_snapshot_archive) =
        LocalCluster::wait_for_next_incremental_snapshot(
            &cluster,
            &validator1_config.full_snapshot_archives_dir,
            &validator1_config.incremental_snapshot_archives_dir,
            Some(Duration::from_secs(5 * 60)),
        );
    debug!(
        "snapshot archives:\n\tfull: {full_snapshot_archive:?}\n\tincr: \
         {incremental_snapshot_archive:?}"
    );
    info!("Waiting for validator1 to create snapshots... DONE");

    // These structs are used to provide better error logs if the asserts below are violated.
    // The `allow(dead_code)` annotation is to appease clippy, which thinks the field is unused...
    #[allow(dead_code)]
    #[derive(Debug)]
    struct SnapshotSlot(Slot);

    for (i, other_validator_config) in [(2, &validator2_config), (3, &validator3_config)] {
        info!("Checking if validator{i} has the same snapshots as validator1...");
        let timer = Instant::now();
        loop {
            if let Some(other_full_snapshot_slot) =
                snapshot_paths::get_highest_full_snapshot_archive_slot(
                    &other_validator_config.full_snapshot_archives_dir,
                )
            {
                let other_incremental_snapshot_slot =
                    snapshot_paths::get_highest_incremental_snapshot_archive_slot(
                        &other_validator_config.incremental_snapshot_archives_dir,
                        other_full_snapshot_slot,
                    );
                if other_full_snapshot_slot >= full_snapshot_archive.slot()
                    && other_incremental_snapshot_slot >= Some(incremental_snapshot_archive.slot())
                {
                    break;
                }
            }
            assert!(
                timer.elapsed() < Duration::from_secs(60),
                "It should not take longer than 60 seconds to take snapshots",
            );
            std::thread::yield_now();
        }
        let other_full_snapshot_archives = snapshot_paths::full_snapshot_archives_iter(
            other_validator_config.full_snapshot_archives_dir.path(),
        )
        .collect::<Vec<_>>();
        debug!("validator{i} full snapshot archives: {other_full_snapshot_archives:?}");
        assert!(
            other_full_snapshot_archives
                .iter()
                .any(
                    |other_full_snapshot_archive| other_full_snapshot_archive.slot()
                        == full_snapshot_archive.slot()
                        && other_full_snapshot_archive.hash() == full_snapshot_archive.hash()
                ),
            "full snapshot archive does not match!\n  validator1: {:?}\n  validator{i}: {:?}",
            (
                SnapshotSlot(full_snapshot_archive.slot()),
                full_snapshot_archive.hash(),
            ),
            other_full_snapshot_archives
                .iter()
                .sorted_unstable()
                .rev()
                .map(|snap| (SnapshotSlot(snap.slot()), snap.hash()))
                .collect::<Vec<_>>(),
        );

        info!("Checking if validator{i} has the same snapshots as validator1... DONE");
    }
}

/// Test fastboot to ensure a node can boot in case it crashed while archiving a full snapshot
///
/// 1. Start a node and wait for it to take at least two full snapshots and one more
///    bank snapshot POST afterwards (for simplicity, wait for 2 full and 1 incremental).
/// 2. To simulate a node crashing while archiving a full snapshot, stop the node and
///    then delete the latest full snapshot archive.
/// 3. Restart the node.  This should succeed, and boot from the older full snapshot archive,
///    *not* the latest bank snapshot POST.
/// 4. Take another incremental snapshot.  This ensures the correct snapshot was loaded,
///    AND ensures the correct accounts hashes are present (which are needed when making
///    the bank snapshot POST for the new incremental snapshot).
#[test]
#[serial]
fn test_boot_from_local_state_missing_archive() {
    agave_logger::setup_with_default(RUST_LOG_FILTER);
    const FULL_SNAPSHOT_INTERVAL: SnapshotInterval =
        SnapshotInterval::Slots(NonZeroU64::new(20).unwrap());
    const INCREMENTAL_SNAPSHOT_INTERVAL: SnapshotInterval =
        SnapshotInterval::Slots(NonZeroU64::new(10).unwrap());

    let validator_config =
        SnapshotValidatorConfig::new(FULL_SNAPSHOT_INTERVAL, INCREMENTAL_SNAPSHOT_INTERVAL, 7);

    let mut cluster_config = ClusterConfig {
        node_stakes: vec![100 * DEFAULT_NODE_STAKE],
        validator_configs: make_identical_validator_configs(&validator_config.validator_config, 1),
        ..ClusterConfig::default()
    };
    let mut cluster = LocalCluster::new(&mut cluster_config, SocketAddrSpace::Unspecified);

    // we need two full snapshots and an incremental snapshot for this test
    info!("Waiting for validator to create snapshots...");
    LocalCluster::wait_for_next_full_snapshot(
        &cluster,
        &validator_config.full_snapshot_archives_dir,
        Some(Duration::from_secs(5 * 60)),
    );
    LocalCluster::wait_for_next_full_snapshot(
        &cluster,
        &validator_config.full_snapshot_archives_dir,
        Some(Duration::from_secs(5 * 60)),
    );
    LocalCluster::wait_for_next_incremental_snapshot(
        &cluster,
        &validator_config.full_snapshot_archives_dir,
        &validator_config.incremental_snapshot_archives_dir,
        Some(Duration::from_secs(5 * 60)),
    );
    debug!(
        "snapshot archives:\n\tfull: {:?}\n\tincr: {:?}",
        snapshot_paths::full_snapshot_archives_iter(
            validator_config.full_snapshot_archives_dir.path()
        )
        .collect::<Vec<_>>(),
        snapshot_paths::incremental_snapshot_archives_iter(
            validator_config.incremental_snapshot_archives_dir.path()
        )
        .collect::<Vec<_>>(),
    );
    info!("Waiting for validator to create snapshots... DONE");

    // now delete the latest full snapshot archive and restart, to simulate a crash while archiving
    // a full snapshot package
    info!("Stopping validator...");
    let validator_pubkey = cluster.get_node_pubkeys()[0];
    let mut validator_info = cluster.exit_node(&validator_pubkey);
    info!("Stopping validator... DONE");

    info!("Deleting latest full snapshot archive...");
    let highest_full_snapshot = snapshot_paths::get_highest_full_snapshot_archive_info(
        validator_config.full_snapshot_archives_dir.path(),
    )
    .unwrap();
    fs::remove_file(highest_full_snapshot.path()).unwrap();
    info!("Deleting latest full snapshot archive... DONE");

    info!("Restarting validator...");
    // if we set this to `Never`, the validator should not boot
    validator_info.config.use_snapshot_archives_at_startup =
        UseSnapshotArchivesAtStartup::WhenNewest;
    cluster.restart_node(
        &validator_pubkey,
        validator_info,
        SocketAddrSpace::Unspecified,
    );
    info!("Restarting validator... DONE");

    // ensure we can create new incremental snapshots, since that is what used to fail
    info!("Waiting for validator to create snapshots...");
    LocalCluster::wait_for_next_incremental_snapshot(
        &cluster,
        &validator_config.full_snapshot_archives_dir,
        &validator_config.incremental_snapshot_archives_dir,
        Some(Duration::from_secs(5 * 60)),
    );
    info!("Waiting for validator to create snapshots... DONE");
}
/// Randomizes block verification methods across validators and proves the mixed-method
/// cluster still interoperates end to end.
#[test]
#[serial]
fn test_randomly_mixed_block_verification_methods_between_bootstrap_and_not() {
    // tailored logging just to see two block verification methods are working correctly
    agave_logger::setup_with_default(
        "solana_metrics::metrics=warn,solana_core=warn,\
         solana_runtime::installed_scheduler_pool=trace,solana_ledger::blockstore_processor=debug,\
         info",
    );

    let num_nodes = BlockVerificationMethod::COUNT;
    let mut config =
        ClusterConfig::new_with_equal_stakes(num_nodes, DEFAULT_MINT_LAMPORTS, DEFAULT_NODE_STAKE);

    // Overwrite block_verification_method with shuffled variants
    let mut methods = BlockVerificationMethod::iter().collect::<Vec<_>>();
    methods.shuffle(&mut rand::rng());
    for (validator_config, method) in config.validator_configs.iter_mut().zip_eq(methods) {
        validator_config.block_verification_method = method;
    }

    let local = LocalCluster::new(&mut config, SocketAddrSpace::Unspecified);
    cluster_tests::spend_and_verify_all_nodes(
        &local.entry_point_info,
        &local.funding_keypair,
        num_nodes,
        HashSet::new(),
        SocketAddrSpace::Unspecified,
        &cluster_tests::TpuSender::new(),
    );
}

/// Randomizes block production methods across validators and checks the heterogeneous
/// producers still land transactions and maintain cluster progress.
#[test]
#[serial]
fn test_randomly_mixed_block_production_methods_between_bootstrap_and_not() {
    // tailored logging just to see two block production methods are working correctly
    agave_logger::setup_with_default(
        "solana_metrics::metrics=warn,solana_core=warn,\
         solana_runtime::installed_scheduler_pool=trace,solana_ledger::blockstore_processor=debug,\
         info",
    );

    let num_nodes = BlockProductionMethod::COUNT;
    let mut config =
        ClusterConfig::new_with_equal_stakes(num_nodes, DEFAULT_MINT_LAMPORTS, DEFAULT_NODE_STAKE);

    // Overwrite block_production_method with shuffled variants
    let mut methods = BlockProductionMethod::iter().collect::<Vec<_>>();
    methods.shuffle(&mut rand::rng());
    for (validator_config, method) in config.validator_configs.iter_mut().zip_eq(methods) {
        validator_config.block_production_method = method;
    }

    let local = LocalCluster::new(&mut config, SocketAddrSpace::Unspecified);
    cluster_tests::spend_and_verify_all_nodes(
        &local.entry_point_info,
        &local.funding_keypair,
        num_nodes,
        HashSet::new(),
        SocketAddrSpace::Unspecified,
        &cluster_tests::TpuSender::new(),
    );
}
fn test_alpenglow_nodes_basic(num_nodes: usize, num_offline_nodes: usize) {
    test_alpenglow_nodes_with_config(
        num_nodes,
        num_offline_nodes,
        ValidatorConfig::default_for_test(),
    );
}

fn test_alpenglow_nodes_with_config(
    num_nodes: usize,
    num_offline_nodes: usize,
    mut validator_config: ValidatorConfig,
) {
    agave_logger::setup_with_default(AG_DEBUG_LOG_FILTER);
    let validator_keys = (0..num_nodes)
        .map(|i| {
            (
                ValidatorKeys {
                    node_keypair: Arc::new(keypair_from_seed(&[i as u8; 32]).unwrap()),
                    vote_keypair: Arc::new(Keypair::new()),
                },
                true,
            )
        })
        .collect::<Vec<_>>();
    validator_config.wait_for_supermajority = Some(0);

    let mut config = ClusterConfig {
        validator_configs: make_identical_validator_configs(&validator_config, num_nodes),
        validator_keys: Some(validator_keys.clone()),
        node_stakes: vec![DEFAULT_NODE_STAKE; num_nodes],
        slots_per_epoch: MINIMUM_SLOTS_PER_EPOCH * 2,
        stakers_slot_offset: MINIMUM_SLOTS_PER_EPOCH * 2,
        poh_config: PohConfig {
            target_tick_duration: PohConfig::default().target_tick_duration,
            hashes_per_tick: None,
            target_tick_count: None,
        },
        ..ClusterConfig::default()
    };
    let mut cluster = LocalCluster::new(&mut config, SocketAddrSpace::Unspecified);
    assert_eq!(cluster.validators.len(), num_nodes);

    // Check transactions land
    cluster_tests::spend_and_verify_all_nodes(
        &cluster.entry_point_info,
        &cluster.funding_keypair,
        num_nodes,
        HashSet::new(),
        SocketAddrSpace::Unspecified,
        &cluster_tests::TpuSender::new(),
    );

    if num_offline_nodes > 0 {
        // Bring nodes offline
        info!("Shutting down {num_offline_nodes} nodes");
        for (key, _) in validator_keys.iter().take(num_offline_nodes) {
            cluster.exit_node(&key.node_keypair.pubkey());
        }
    }

    // Check for new roots
    cluster.check_for_new_roots(
        16,
        &format!("test_{num_nodes}_nodes_alpenglow"),
        SocketAddrSpace::Unspecified,
    );
}

/// Minimal single-node Alpenglow smoke test for boot, transaction execution, and root
/// production without quorum or repair involvement.
#[test]
#[serial]
fn test_alpenglow_1() {
    const NUM_NODES: usize = 1;
    test_alpenglow_nodes_basic(NUM_NODES, 0);
}

/// Multi-node Alpenglow baseline where every validator stays online.
/// This does not cover offline-node or restart behavior.
#[test]
#[serial]
fn test_alpenglow_4() {
    const NUM_NODES: usize = 4;
    test_alpenglow_nodes_basic(NUM_NODES, 0);
}

/// Checks that a four-node Alpenglow cluster still roots after taking one validator
/// offline, which adds liveness-under-loss coverage beyond the all-online baseline.
#[test]
#[serial]
fn test_alpenglow_4_1_offline() {
    const NUM_NODES: usize = 4;
    const NUM_OFFLINE: usize = 1;
    test_alpenglow_nodes_basic(NUM_NODES, NUM_OFFLINE);
}

/// Runs the four-node baseline with reward recovery enabled on every validator, so leaders admit
/// certificates and raw vote signatures for rewards and run the recovery threads while the
/// cluster lands transactions and roots.
#[test]
#[serial]
fn test_alpenglow_4_reward_recovery() {
    const NUM_NODES: usize = 4;
    test_alpenglow_nodes_with_config(
        NUM_NODES,
        0,
        ValidatorConfig {
            enable_reward_recovery: true,
            ..ValidatorConfig::default_for_test()
        },
    );
}

/// Basic restart coverage for the Alpenglow execution path.
/// This does not cover the legacy restart path.
#[test]
#[serial]
fn test_restart_node_alpenglow() {
    agave_logger::setup_with_default(AG_DEBUG_LOG_FILTER);
    let slots_per_epoch = MINIMUM_SLOTS_PER_EPOCH * 2;
    let ticks_per_slot = 16;
    let validator_config = ValidatorConfig::default_for_test();
    let mut cluster = LocalCluster::new(
        &mut ClusterConfig {
            node_stakes: vec![DEFAULT_NODE_STAKE],
            validator_configs: vec![safe_clone_config(&validator_config)],
            ticks_per_slot,
            slots_per_epoch,
            stakers_slot_offset: slots_per_epoch,
            skip_warmup_slots: true,
            ..ClusterConfig::default()
        },
        SocketAddrSpace::Unspecified,
    );
    let nodes = cluster.get_node_pubkeys();
    cluster_tests::sleep_n_epochs(
        1.0,
        &cluster.genesis_config.poh_config,
        ticks_per_slot,
        slots_per_epoch,
    );
    info!("Restarting node");
    cluster.exit_restart_node(&nodes[0], validator_config, SocketAddrSpace::Unspecified);
    cluster_tests::sleep_n_epochs(
        0.5,
        &cluster.genesis_config.poh_config,
        ticks_per_slot,
        slots_per_epoch,
    );
    cluster_tests::send_many_transactions(
        &cluster.entry_point_info,
        &cluster.funding_keypair,
        &cluster_tests::TpuSender::new(),
        10,
        1,
    );
}

/// We start 2 nodes, where the first node A holds 90% of the stake.
/// We let A run by itself, and ensure that B can join and rejoin the network
/// through repair.
/// Verifies that a low-stake Alpenglow validator can fall behind, rejoin through repair,
/// and resume notarized voting in an imbalanced-stake network.
#[test]
#[serial]
fn test_alpenglow_imbalanced_stakes_catchup() {
    agave_logger::setup_with_default(AG_DEBUG_LOG_FILTER);
    // Create node stakes
    let slots_per_epoch = 512;

    let total_stake = 2 * DEFAULT_NODE_STAKE;
    let tenth_stake = total_stake / 10;
    let node_a_stake = 9 * tenth_stake;
    let node_b_stake = total_stake - node_a_stake;

    let node_stakes = vec![node_a_stake, node_b_stake];
    let num_nodes = node_stakes.len();

    // Create leader schedule with A and B as leader 72/28
    let (leader_schedule, validator_keys) =
        create_custom_leader_schedule_with_random_keys(&[72, 28]);

    let leader_schedule = FixedSchedule {
        leader_schedule: Arc::new(leader_schedule),
    };

    let node_pubkeys = validator_keys
        .iter()
        .map(|key| key.node_keypair.pubkey())
        .collect::<Vec<_>>();
    let listener_keypair = Keypair::new();
    let listener_pubkey = listener_keypair.pubkey();

    // Create our UDP socket to listen to votes
    let vote_listener_addr = bind_to_localhost_unique().unwrap();

    let mut validator_config = ValidatorConfig::default_for_test();
    validator_config.fixed_leader_schedule = Some(leader_schedule);
    validator_config.votor_peer_overrides = Arc::new(ArcSwap::from_pointee(HashMap::from([(
        listener_pubkey,
        Some(vote_listener_addr.local_addr().unwrap()),
    )])));
    validator_config.wait_for_supermajority = Some(0);

    // Cluster config
    let mut cluster_config = ClusterConfig {
        mint_lamports: DEFAULT_MINT_LAMPORTS + node_stakes.iter().sum::<u64>(),
        node_stakes: node_stakes.clone(),
        validator_configs: make_identical_validator_configs(&validator_config, num_nodes),
        validator_keys: Some(
            validator_keys
                .iter()
                .cloned()
                .zip(iter::repeat_with(|| true))
                .collect(),
        ),
        slots_per_epoch,
        stakers_slot_offset: slots_per_epoch,
        skip_warmup_slots: true,
        ..ClusterConfig::default()
    };

    // Create local cluster
    let mut cluster = LocalCluster::new(&mut cluster_config, SocketAddrSpace::Unspecified);

    // Ensure all nodes are voting
    cluster.check_for_new_processed(
        8,
        "test_alpenglow_imbalanced_stakes_catchup",
        SocketAddrSpace::Unspecified,
    );

    info!("exiting node B");
    let b_info = cluster.exit_node(&node_pubkeys[1]);

    // Let A make roots by itself
    cluster.check_for_new_roots(
        8,
        "test_alpenglow_imbalanced_stakes_catchup",
        SocketAddrSpace::Unspecified,
    );

    info!("restarting node B");
    cluster.restart_node(&node_pubkeys[1], b_info, SocketAddrSpace::Unspecified);

    cluster.check_for_new_notarized_votes(
        16,
        "test_alpenglow_imbalanced_stakes_catchup",
        SocketAddrSpace::Unspecified,
        vote_listener_addr,
        listener_keypair,
    );
}

/// We start 2 nodes, where the first node A holds 90% of the stake.
/// B has turbine disabled, and receives duplicate blocks through eager repair.
/// However, through informed repair it is able to repair the correct blocks and keep up with A.
#[test]
#[serial]
fn test_alpenglow_basic_equivocation() {
    agave_logger::setup_with_default(AG_DEBUG_LOG_FILTER);
    // Create node stakes
    let slots_per_epoch = 512;

    let total_stake = 2 * DEFAULT_NODE_STAKE;
    let tenth_stake = total_stake / 10;
    let node_a_stake = 9 * tenth_stake;
    let node_b_stake = total_stake - node_a_stake;

    let node_stakes = vec![node_a_stake, node_b_stake];

    // Create leader schedule with A as the leader
    let (leader_schedule, validator_keys) = create_custom_leader_schedule_with_random_keys(&[4, 0]);

    let leader_schedule = FixedSchedule {
        leader_schedule: Arc::new(leader_schedule),
    };

    let mut a_validator_config = ValidatorConfig::default_for_test();
    a_validator_config.wait_for_supermajority = Some(0);
    a_validator_config.fixed_leader_schedule = Some(leader_schedule);

    let node_b_turbine_mode = TurbineMode::new(TurbineModeKind::TurbineDisabled);
    let mut b_validator_config = safe_clone_config(&a_validator_config);
    b_validator_config.turbine_mode = node_b_turbine_mode.clone();
    let unstaked_turbine_mode = TurbineMode::new(TurbineModeKind::TurbineDisabled);

    // Equivocate every other slot, one shred per FEC set
    let last_duplicate = 20;
    let duplicate_frequency = 2;
    let expected_duplicate_blocks = (last_duplicate / duplicate_frequency) as usize - 1;
    a_validator_config.repair_handler_type = RepairHandlerType::Malicious(MaliciousRepairConfig {
        bad_shred_slot_frequency: Some(duplicate_frequency),
        bad_shred_index_frequency: Some(DATA_SHREDS_PER_FEC_BLOCK as u64),
        slot_range: Some((0, last_duplicate)),
    });

    // Cluster config
    let mut cluster_config = ClusterConfig {
        mint_lamports: DEFAULT_MINT_LAMPORTS + total_stake,
        node_stakes: node_stakes.clone(),
        validator_configs: vec![a_validator_config, b_validator_config],
        validator_keys: Some(
            validator_keys
                .iter()
                .cloned()
                .zip(iter::repeat_with(|| true))
                .collect(),
        ),
        num_listeners: 1,
        listener_turbine_mode: Some(unstaked_turbine_mode.clone()),
        slots_per_epoch,
        stakers_slot_offset: slots_per_epoch,
        skip_warmup_slots: true,
        ..ClusterConfig::default()
    };

    // Create local cluster
    let cluster = LocalCluster::new(&mut cluster_config, SocketAddrSpace::Unspecified);
    let node_b_pubkey = validator_keys[1].node_keypair.pubkey();

    // Check to make sure the low staked node observes duplicate blocks
    let start = Instant::now();
    loop {
        let blockstore = Blockstore::open_with_options(
            &cluster.ledger_path(&node_b_pubkey),
            BlockstoreOptions {
                access_type: AccessType::ReadOnly,
                ..BlockstoreOptions::default()
            },
        )
        .unwrap();
        let total_duplicate_blocks_observed = (1..=last_duplicate)
            .filter(|slot| blockstore.has_duplicate_shreds_in_slot(*slot))
            .count();
        if total_duplicate_blocks_observed >= expected_duplicate_blocks {
            break;
        }
        if start.elapsed() > Duration::from_secs(60) {
            panic!(
                "Expected to see {expected_duplicate_blocks} in 60 seconds but only saw \
                 {total_duplicate_blocks_observed}"
            );
        }
        sleep(Duration::from_secs(1));
    }

    // Turn turbine back on, now the low-staked and unstaked nodes will be able to catch up
    node_b_turbine_mode.set(TurbineModeKind::Enabled);
    unstaked_turbine_mode.set(TurbineModeKind::Enabled);

    // Ensure all nodes (including the unstaked) are rooting
    // Although the low staked nodes might be behind while the leader is equivocating,
    // once the leader stops equivocating it will be able to catch up
    cluster.check_for_new_roots(
        32,
        "test_alpenglow_basic_equivocation",
        SocketAddrSpace::Unspecified,
    );
}
