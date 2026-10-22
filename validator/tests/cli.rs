use {
    agave_validator::{ledger_lockfile, lock_ledger},
    assert_cmd::prelude::*,
    solana_keypair::{Keypair, write_keypair_file},
    std::{fs, process::Command},
    tempfile::TempDir,
};

#[test]
fn test_use_the_same_path_for_accounts_and_snapshots() {
    let temp_dir = TempDir::new().unwrap();
    let temp_dir_path = temp_dir.path();

    let id_json_path = temp_dir_path.join("id.json");
    let id_json_str = id_json_path.to_str().unwrap();

    let keypair = Keypair::new();
    write_keypair_file(&keypair, id_json_str).unwrap();

    let temp_dir_str = temp_dir_path.to_str().unwrap();
    let ledger_path = temp_dir_path.join("ledger");

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!(env!("CARGO_PKG_NAME")));
    cmd.args([
        "--identity",
        id_json_str,
        "--ledger",
        ledger_path.to_str().unwrap(),
        "--log",
        "-",
        "--no-voting",
        "--no-xdp",
        "--accounts",
        temp_dir_str,
        "--snapshots",
        temp_dir_str,
    ]);
    cmd.assert().failure().stderr(predicates::str::contains(
        "the --accounts and --snapshots paths must be unique",
    ));
}

#[test]
fn test_locked_ledger() {
    let temp_dir = TempDir::new().unwrap();
    let temp_dir_path = temp_dir.path();

    let id_json_path = temp_dir_path.join("id.json");
    let id_json_str = id_json_path.to_str().unwrap();
    write_keypair_file(&Keypair::new(), id_json_str).unwrap();

    let ledger_path = temp_dir_path.join("ledger");
    fs::create_dir(&ledger_path).unwrap();
    let mut ledger_lock = ledger_lockfile(&ledger_path);
    let _ledger_write_guard = lock_ledger(&ledger_path, &mut ledger_lock);

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!(env!("CARGO_PKG_NAME")));
    cmd.args([
        "--identity",
        id_json_str,
        "--ledger",
        ledger_path.to_str().unwrap(),
    ]);
    cmd.assert().failure().stdout(predicates::str::contains(
        "Check if another validator is running",
    ));
    let entries: Vec<_> = fs::read_dir(&ledger_path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(
        entries,
        ["ledger.lock"],
        "a validator that fails to lock the ledger must not modify it"
    );
}

#[cfg(unix)]
#[test]
fn test_locked_ledger_symlink() {
    let temp_dir = TempDir::new().unwrap();
    let temp_dir_path = temp_dir.path();

    let id_json_path = temp_dir_path.join("id.json");
    let id_json_str = id_json_path.to_str().unwrap();
    write_keypair_file(&Keypair::new(), id_json_str).unwrap();

    let ledger_path = temp_dir_path.join("ledger");
    fs::create_dir(&ledger_path).unwrap();
    let mut ledger_lock = ledger_lockfile(&ledger_path);
    let _ledger_write_guard = lock_ledger(&ledger_path, &mut ledger_lock);
    let symlink_path = temp_dir_path.join("symlink");
    std::os::unix::fs::symlink(&ledger_path, &symlink_path).unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!(env!("CARGO_PKG_NAME")));
    cmd.args([
        "--identity",
        id_json_str,
        "--ledger",
        symlink_path.to_str().unwrap(),
    ]);
    cmd.assert()
        .failure()
        .stdout(predicates::str::contains(format!(
            "Unable to lock {} directory",
            ledger_path.canonicalize().unwrap().display()
        )));
}

#[test]
fn test_ledger_path_cannot_be_created() {
    let temp_dir = TempDir::new().unwrap();
    let temp_dir_path = temp_dir.path();

    let id_json_path = temp_dir_path.join("id.json");
    let id_json_str = id_json_path.to_str().unwrap();
    write_keypair_file(&Keypair::new(), id_json_str).unwrap();

    let ledger_path = id_json_path.join("ledger");

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!(env!("CARGO_PKG_NAME")));
    cmd.args([
        "--identity",
        id_json_str,
        "--ledger",
        ledger_path.to_str().unwrap(),
    ]);
    cmd.assert().failure().stdout(predicates::str::contains(
        "failed to create and canonicalize ledger path",
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn test_conflicting_xdp_copy_mode_flags() {
    for flags in [
        ["--xdp-zero-copy", "--no-xdp-zero-copy"],
        ["--no-xdp-zero-copy", "--xdp-zero-copy"],
        ["--no-xdp", "--no-xdp-zero-copy"],
        ["--no-xdp-zero-copy", "--no-xdp"],
    ] {
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!(env!("CARGO_PKG_NAME")));
        cmd.args(flags);
        cmd.assert()
            .failure()
            .stderr(predicates::str::contains("cannot be used with"));
    }
}
