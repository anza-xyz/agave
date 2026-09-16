use {
    solana_entry::entry::next_versioned_entry,
    solana_hash::Hash,
    solana_instruction::Instruction,
    solana_ledger::{
        blockstore, blockstore::Blockstore, create_new_tmp_ledger_auto_delete,
        genesis_utils::create_genesis_config, get_tmp_ledger_path_auto_delete,
    },
    solana_pubkey::Pubkey,
    solana_transaction::Transaction,
    std::{
        path::Path,
        process::{Command, Output},
    },
};

fn run_ledger_tool(args: &[&str]) -> Output {
    Command::new(assert_cmd::cargo::cargo_bin!(env!("CARGO_PKG_NAME")))
        .args(args)
        .env("RUST_LOG", "error")
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
fn latest_optimistic_slots_reports_sanitization_errors() {
    let instruction = Instruction::new_with_bytes(solana_vote_program::id(), &[], vec![]);
    let vote_transaction = Transaction::new_with_payer(&[instruction], Some(&Pubkey::new_unique()));
    let mut malformed_transaction = vote_transaction.clone();
    malformed_transaction.message.instructions[0].program_id_index = u8::MAX;

    let ledger_path = get_tmp_ledger_path_auto_delete!();
    let blockstore = Blockstore::open(ledger_path.path()).unwrap();
    let entry = next_versioned_entry(
        &Hash::default(),
        1,
        vec![vote_transaction.into(), malformed_transaction.into()],
    );
    let shreds = blockstore::entries_to_test_shreds(&[entry], 1, 0, true, 0);
    blockstore.insert_shreds(shreds, false).unwrap();
    blockstore
        .insert_optimistic_slot(1, &Hash::default(), 0)
        .unwrap();

    let output = run_ledger_tool(&[
        "-l",
        ledger_path.path().to_str().unwrap(),
        "latest-optimistic-slots",
        "--exclude-vote-only-slots",
    ]);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("Failed to sanitize transaction 1 in slot 1: IndexOutOfBounds"),
        "{stderr}"
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Vote Only: false"), "{stdout}");
}
