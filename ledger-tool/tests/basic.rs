use {
    agave_snapshots::snapshot_config::SnapshotConfig,
    solana_entry::block_component::{
        BlockComponent, VersionedBlockFooter, VersionedBlockHeader, VersionedBlockMarker,
    },
    solana_ledger::{
        blockstore,
        blockstore::Blockstore,
        blockstore_meta::BlockLocation,
        create_new_tmp_ledger_auto_delete,
        genesis_utils::create_genesis_config,
        get_tmp_ledger_path_auto_delete,
        shred::{ShredFlags, wire},
    },
    solana_runtime::{
        genesis_utils::{
            ValidatorVoteKeypairs, create_genesis_config_with_alpenglow_vote_accounts,
        },
        snapshot_bank_utils,
    },
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

fn component_reference_ticks(
    blockstore: &Blockstore,
    slot: u64,
    component_ranges: &[std::ops::Range<u32>],
) -> Vec<u8> {
    component_ranges
        .iter()
        .map(|range| {
            let shred = blockstore
                .get_data_shred(slot, u64::from(range.start))
                .unwrap()
                .unwrap();
            (wire::get_flags(&shred).unwrap() & ShredFlags::SHRED_TICK_REFERENCE_MASK).bits()
        })
        .collect()
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

#[test]
fn create_snapshot_alpenglow_child_uses_components_and_dmr() {
    const SOURCE_SLOT: u64 = 0;
    const CHILD_SLOT: u64 = 1;
    const GRANDCHILD_SLOT: u64 = 2;
    const TICKS_PER_SLOT: u64 = 8;

    let validator_keypairs = [ValidatorVoteKeypairs::new_rand()];
    let mut genesis_config = create_genesis_config_with_alpenglow_vote_accounts(
        1_000_000_000,
        &validator_keypairs,
        vec![1_000],
    )
    .genesis_config;
    // Slot zero is the Alpenglow genesis block and still uses the legacy entry format. Keep its
    // PoH deterministic and cheap; the snapshot command switches the child to sleep-mode PoH.
    genesis_config.ticks_per_slot = TICKS_PER_SLOT;
    genesis_config.poh_config.hashes_per_tick = Some(1);

    let (ledger_path, parent_last_blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);
    let expected_parent_block_id = {
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();
        blockstore
            .get_last_shred_merkle_root(SOURCE_SLOT)
            .unwrap()
            .unwrap()
    };

    let output_directory = tempfile::tempdir().unwrap();
    let source_slot = SOURCE_SLOT.to_string();
    let ledger_path_string = ledger_path.path().to_str().unwrap();
    let output_directory_string = output_directory.path().to_str().unwrap();
    let output = run_ledger_tool(&[
        "-l",
        ledger_path_string,
        "create-snapshot",
        &source_slot,
        output_directory_string,
        "--hashes-per-tick",
        "sleep",
        "--snapshot-archive-format",
        "lz4",
    ]);
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let snapshot_config = SnapshotConfig {
        full_snapshot_archives_dir: output_directory.path().to_path_buf(),
        incremental_snapshot_archives_dir: output_directory.path().to_path_buf(),
        use_direct_io: false,
        use_registered_io_uring_buffers: false,
        ..SnapshotConfig::new_load_only()
    };
    let fields = snapshot_bank_utils::bank_fields_from_snapshot_archives(&snapshot_config).unwrap();
    assert_eq!(fields.slot, CHILD_SLOT);
    assert_eq!(fields.hashes_per_tick, None);
    assert_eq!(fields.tick_height, fields.max_tick_height);

    let blockstore = Blockstore::open(ledger_path.path()).unwrap();
    let meta = blockstore.meta(CHILD_SLOT).unwrap().unwrap();
    assert!(meta.is_full());
    assert_eq!(meta.parent_slot, Some(SOURCE_SLOT));
    assert_eq!(meta.parent_block_id, expected_parent_block_id);

    let (components, component_ranges, is_full) = blockstore
        .get_slot_components_with_shred_info(CHILD_SLOT, 0, false)
        .unwrap();
    assert!(is_full);
    assert_eq!(component_ranges.len(), 3);
    assert_eq!(components.len(), 3);
    assert_eq!(
        component_reference_ticks(&blockstore, CHILD_SLOT, &component_ranges),
        [0, TICKS_PER_SLOT as u8 - 1, TICKS_PER_SLOT as u8]
    );

    let BlockComponent::BlockMarker(VersionedBlockMarker::V1(header_marker)) = &components[0]
    else {
        panic!("first component is not a block header");
    };
    let VersionedBlockHeader::V1(header) = header_marker
        .as_block_header()
        .expect("first component is not a block header");
    assert_eq!(header.parent_slot, SOURCE_SLOT);
    assert_eq!(header.parent_block_id, expected_parent_block_id);

    let BlockComponent::BlockMarker(VersionedBlockMarker::V1(footer_marker)) = &components[1]
    else {
        panic!("second component is not a block footer");
    };
    let VersionedBlockFooter::V1(footer) = footer_marker
        .as_block_footer()
        .expect("second component is not a block footer");
    assert_eq!(footer.bank_hash, fields.hash);
    assert_eq!(
        footer.block_user_agent,
        format!("agave-ledger-tool/{}", solana_version::version!()).into_bytes()
    );
    assert!(footer.block_final_cert.is_none());
    assert!(footer.skip_reward_cert.is_none());
    assert!(footer.notar_reward_cert.is_none());

    let BlockComponent::EntryBatch(entries) = &components[2] else {
        panic!("final component is not an alpentick batch");
    };
    assert_eq!(entries.len(), 1);
    let alpentick = &entries[0];
    assert!(alpentick.is_tick());
    assert_eq!(alpentick.num_hashes, 1);
    assert!(alpentick.transactions.is_empty());
    assert_eq!(
        alpentick,
        &solana_entry::entry::Entry::new(&parent_last_blockhash, 1, vec![])
    );
    assert_eq!(fields.blockhash_queue.last_hash(), alpentick.hash);
    assert_eq!(
        fields.blockhash_queue.get_hash_age(&alpentick.hash),
        Some(0)
    );
    assert_eq!(
        fields.blockhash_queue.get_hash_age(&parent_last_blockhash),
        Some(1)
    );

    let child_block_id = blockstore
        .get_double_merkle_root(CHILD_SLOT, BlockLocation::Original)
        .unwrap()
        .unwrap();
    assert_eq!(fields.block_id, Some(child_block_id));
    let child_last_shred_merkle_root = blockstore
        .get_last_shred_merkle_root(CHILD_SLOT)
        .unwrap()
        .unwrap();
    drop(blockstore);

    // Replaying from genesis exercises the header/footer/alpentick parser and verifies that
    // applying the serialized footer fields computes the same bank hash as the snapshot bank.
    let child_slot = CHILD_SLOT.to_string();
    let verify = run_ledger_tool(&[
        "-l",
        ledger_path_string,
        "verify",
        "--no-snapshot",
        "--halt-at-slot",
        &child_slot,
        "--abort-on-invalid-block",
    ]);
    assert!(
        verify.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&verify.stdout),
        String::from_utf8_lossy(&verify.stderr),
    );

    // Generate one more child so the parent is an established Alpenglow block. Its DMR belongs
    // in the header while its final FEC Merkle root seeds the new shred chain; these hashes are
    // intentionally distinct.
    let second_output_directory = tempfile::tempdir().unwrap();
    let second_output_directory_string = second_output_directory.path().to_str().unwrap();
    let output = run_ledger_tool(&[
        "-l",
        ledger_path_string,
        "create-snapshot",
        &child_slot,
        second_output_directory_string,
        "--hashes-per-tick",
        "sleep",
        "--snapshot-archive-format",
        "lz4",
    ]);
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let second_snapshot_config = SnapshotConfig {
        full_snapshot_archives_dir: second_output_directory.path().to_path_buf(),
        incremental_snapshot_archives_dir: second_output_directory.path().to_path_buf(),
        use_direct_io: false,
        use_registered_io_uring_buffers: false,
        ..SnapshotConfig::new_load_only()
    };
    let second_fields =
        snapshot_bank_utils::bank_fields_from_snapshot_archives(&second_snapshot_config).unwrap();
    assert_eq!(second_fields.slot, GRANDCHILD_SLOT);

    let blockstore = Blockstore::open(ledger_path.path()).unwrap();
    let meta = blockstore.meta(GRANDCHILD_SLOT).unwrap().unwrap();
    assert!(meta.is_full());
    assert_eq!(meta.parent_slot, Some(CHILD_SLOT));
    assert_eq!(meta.parent_block_id, child_block_id);
    let (components, component_ranges, is_full) = blockstore
        .get_slot_components_with_shred_info(GRANDCHILD_SLOT, 0, false)
        .unwrap();
    assert!(is_full);
    assert_eq!(
        component_reference_ticks(&blockstore, GRANDCHILD_SLOT, &component_ranges),
        [0, TICKS_PER_SLOT as u8 - 1, TICKS_PER_SLOT as u8]
    );
    let BlockComponent::BlockMarker(VersionedBlockMarker::V1(header_marker)) = &components[0]
    else {
        panic!("first component is not a block header");
    };
    let VersionedBlockHeader::V1(header) = header_marker
        .as_block_header()
        .expect("first component is not a block header");
    assert_eq!(header.parent_slot, CHILD_SLOT);
    assert_eq!(header.parent_block_id, child_block_id);

    let first_shred = blockstore
        .get_data_shreds_for_slot(GRANDCHILD_SLOT, 0)
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(
        first_shred.chained_merkle_root().unwrap(),
        child_last_shred_merkle_root
    );
    assert_ne!(child_block_id, child_last_shred_merkle_root);

    let grandchild_block_id = blockstore
        .get_double_merkle_root(GRANDCHILD_SLOT, BlockLocation::Original)
        .unwrap()
        .unwrap();
    assert_eq!(second_fields.block_id, Some(grandchild_block_id));
    drop(blockstore);

    let grandchild_slot = GRANDCHILD_SLOT.to_string();
    let verify = run_ledger_tool(&[
        "-l",
        ledger_path_string,
        "verify",
        "--no-snapshot",
        "--halt-at-slot",
        &grandchild_slot,
        "--abort-on-invalid-block",
    ]);
    assert!(
        verify.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&verify.stdout),
        String::from_utf8_lossy(&verify.stderr),
    );

    let verify_snapshot = run_ledger_tool(&[
        "-l",
        ledger_path_string,
        "verify",
        "--full-snapshot-archive-path",
        second_output_directory_string,
        "--halt-at-slot",
        &grandchild_slot,
        "--abort-on-invalid-block",
    ]);
    assert!(
        verify_snapshot.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&verify_snapshot.stdout),
        String::from_utf8_lossy(&verify_snapshot.stderr),
    );
}
