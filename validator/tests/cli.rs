use {
    assert_cmd::prelude::*,
    solana_keypair::{Keypair, write_keypair_file},
    std::process::Command,
    tempfile::TempDir,
};

#[test]
fn test_print_default_config_exits_without_startup_side_effects() {
    let temp_dir = TempDir::new().unwrap();
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!(env!("CARGO_PKG_NAME")));
    cmd.current_dir(temp_dir.path())
        .args(["--print-default-config", "--ledger", "ledger"]);
    cmd.assert()
        .success()
        .stdout(include_str!("../src/commands/run/default_config.toml"));
    let created_file = std::fs::read_dir(temp_dir.path()).unwrap().next();
    assert!(
        created_file.is_none(),
        "printing config created a file: {created_file:?}"
    );
}

#[test]
fn test_use_the_same_path_for_accounts_and_snapshots() {
    let temp_dir = TempDir::new().unwrap();
    let temp_dir_path = temp_dir.path();

    let id_json_path = temp_dir_path.join("id.json");
    let id_json_str = id_json_path.to_str().unwrap();

    let keypair = Keypair::new();
    write_keypair_file(&keypair, id_json_str).unwrap();

    let temp_dir_str = temp_dir_path.to_str().unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!(env!("CARGO_PKG_NAME")));
    cmd.args([
        "--identity",
        id_json_str,
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
