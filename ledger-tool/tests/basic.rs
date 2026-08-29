use {
    agave_snapshots::snapshot_config::SnapshotConfig,
    solana_ledger::{
        blockstore, blockstore::Blockstore, create_new_tmp_ledger_auto_delete,
        genesis_utils::create_genesis_config, get_tmp_ledger_path_auto_delete,
    },
    solana_runtime::{
        bank::Bank, genesis_utils::activate_all_features_alpenglow, snapshot_bank_utils,
    },
    solana_shred_version::compute_shred_version,
    std::{
        path::Path,
        process::{Command, Output},
    },
};

fn run_ledger_tool(args: &[&str]) -> Output {
    Command::new(assert_cmd::cargo::cargo_bin!(env!("CARGO_PKG_NAME")))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn bad_arguments() {
    // At least a ledger path is required
    assert!(!run_ledger_tool(&[]).status.success());

    // Invalid ledger path should fail
    assert!(
        !run_ledger_tool(&["-l", "invalid_ledger", "verify"])
            .status
            .success()
    );
}

fn nominal_test_helper(ledger_path: &str) {
    let output = run_ledger_tool(&["-l", ledger_path, "verify"]);
    assert!(output.status.success());

    let output = run_ledger_tool(&["-l", ledger_path, "print", "-vv"]);
    assert!(output.status.success());
}

#[test]
fn nominal_default() {
    let genesis_config = create_genesis_config(100).genesis_config;
    let (ledger_path, _blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);
    nominal_test_helper(ledger_path.path().to_str().unwrap());
}

#[test]
fn rollback_alpenglow_replaces_existing_child_slot() {
    const SOURCE_SLOT: u64 = 0;
    const ROLLBACK_SLOT: u64 = SOURCE_SLOT + 1;
    const DESCENDANT_SLOT: u64 = ROLLBACK_SLOT + 1;

    // A hard fork changes only a 16-bit version, so avoid the small chance that this test's
    // randomly generated genesis collides with the source version.
    let genesis_config = loop {
        let mut genesis_config = create_genesis_config(100).genesis_config;
        activate_all_features_alpenglow(&mut genesis_config);
        let bank = Bank::new_for_tests(&genesis_config);
        bank.register_hard_fork(ROLLBACK_SLOT);
        if compute_shred_version(&genesis_config.hash(), None)
            != compute_shred_version(&genesis_config.hash(), Some(&bank.hard_forks()))
        {
            break genesis_config;
        }
    };
    let source_shred_version = compute_shred_version(&genesis_config.hash(), None);
    let (ledger_path, _blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);

    let (_, old_entries) = blockstore::make_slot_entries(ROLLBACK_SLOT, SOURCE_SLOT, 10);
    let old_shreds = blockstore::entries_to_test_shreds(
        &old_entries,
        ROLLBACK_SLOT,
        SOURCE_SLOT,
        true,
        source_shred_version,
    );
    let (_, descendant_entries) = blockstore::make_slot_entries(DESCENDANT_SLOT, ROLLBACK_SLOT, 10);
    let descendant_shreds = blockstore::entries_to_test_shreds(
        &descendant_entries,
        DESCENDANT_SLOT,
        ROLLBACK_SLOT,
        true,
        source_shred_version,
    );
    {
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();
        blockstore
            .insert_shreds(
                old_shreds
                    .iter()
                    .chain(&descendant_shreds)
                    .cloned()
                    .collect::<Vec<_>>(),
                false,
            )
            .unwrap();
    }

    let output_directory = tempfile::tempdir().unwrap();
    let output = run_ledger_tool(&[
        "-l",
        ledger_path.path().to_str().unwrap(),
        "create-snapshot",
        &SOURCE_SLOT.to_string(),
        output_directory.path().to_str().unwrap(),
        "--rollback-alpenglow",
        "--enable-capitalization-change",
        "--snapshot-archive-format",
        "lz4",
    ]);
    assert!(
        output.status.success(),
        "ledger-tool failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let snapshot_config = SnapshotConfig {
        full_snapshot_archives_dir: output_directory.path().to_path_buf(),
        incremental_snapshot_archives_dir: output_directory.path().to_path_buf(),
        use_direct_io: false,
        use_registered_io_uring_buffers: false,
        ..SnapshotConfig::default()
    };
    let snapshot_fields =
        snapshot_bank_utils::bank_fields_from_snapshot_archives(&snapshot_config).unwrap();
    let hard_fork_shred_version =
        compute_shred_version(&genesis_config.hash(), Some(&snapshot_fields.hard_forks));

    let verify_output = run_ledger_tool(&[
        "-l",
        ledger_path.path().to_str().unwrap(),
        "verify",
        "--full-snapshot-archive-path",
        output_directory.path().to_str().unwrap(),
        "--halt-at-slot",
        &ROLLBACK_SLOT.to_string(),
    ]);
    assert!(
        verify_output.status.success(),
        "in-place ledger verification failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&verify_output.stdout),
        String::from_utf8_lossy(&verify_output.stderr),
    );

    let blockstore = Blockstore::open(ledger_path.path()).unwrap();
    assert_eq!(blockstore.highest_slot().unwrap(), Some(ROLLBACK_SLOT));
    let rewritten_shreds = blockstore
        .get_data_shreds_for_slot(ROLLBACK_SLOT, 0)
        .unwrap();
    assert!(
        rewritten_shreds
            .iter()
            .all(|shred| shred.version() == hard_fork_shred_version)
    );
    assert_eq!(
        blockstore
            .get_last_shred_merkle_root(ROLLBACK_SLOT)
            .unwrap(),
        snapshot_fields.block_id,
    );

    let backup_path = ledger_path.path().join(format!(
        "rocksdb_backup_{source_shred_version}_{ROLLBACK_SLOT}"
    ));
    let backup_blockstore = Blockstore::open(&backup_path).unwrap();
    assert_eq!(
        backup_blockstore
            .get_data_shreds_for_slot(ROLLBACK_SLOT, 0)
            .unwrap(),
        old_shreds,
    );
    assert_eq!(
        backup_blockstore
            .get_data_shreds_for_slot(DESCENDANT_SLOT, 0)
            .unwrap(),
        descendant_shreds,
    );
}

fn insert_test_shreds(ledger_path: &Path, ending_slot: u64) {
    let blockstore = Blockstore::open(ledger_path).unwrap();
    let (shreds, _) = blockstore::make_many_slot_entries(
        /*start_slot:*/ 0,
        ending_slot,
        /*entries_per_slot:*/ 10,
    );
    blockstore.insert_shreds(shreds, false).unwrap();
}

#[test]
fn ledger_tool_copy_test() {
    let genesis_config = create_genesis_config(100).genesis_config;

    let (ledger_path, _blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);

    const LEDGER_TOOL_COPY_TEST_SHRED_COUNT: u64 = 25;
    const LEDGER_TOOL_COPY_TEST_ENDING_SLOT: u64 = LEDGER_TOOL_COPY_TEST_SHRED_COUNT + 1;
    insert_test_shreds(ledger_path.path(), LEDGER_TOOL_COPY_TEST_ENDING_SLOT);
    let ledger_path = ledger_path.path().to_str().unwrap();

    let target_ledger_path = get_tmp_ledger_path_auto_delete!();
    let target_ledger_path = target_ledger_path.path().to_str().unwrap();
    let output = run_ledger_tool(&[
        "-l",
        ledger_path,
        "copy",
        "--target-ledger",
        target_ledger_path,
        "--ending-slot",
        &(LEDGER_TOOL_COPY_TEST_ENDING_SLOT).to_string(),
    ]);
    assert!(output.status.success());
    for slot_id in 0..LEDGER_TOOL_COPY_TEST_ENDING_SLOT {
        let src_slot_output = run_ledger_tool(&["-l", ledger_path, "slot", &slot_id.to_string()]);

        let dst_slot_output =
            run_ledger_tool(&["-l", target_ledger_path, "slot", &slot_id.to_string()]);
        assert!(src_slot_output.status.success());
        assert!(dst_slot_output.status.success());
        assert!(!src_slot_output.stdout.is_empty());
    }
}
