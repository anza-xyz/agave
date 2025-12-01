use {
    scopeguard::defer,
    serial_test::serial,
    std::{fs, path::Path, process::Command},
};

#[test]
#[serial]
fn test_bump_version() {
    // get current file path and direct to the playground directory
    let current_file_path_str = file!();
    let root_path = fs::canonicalize(
        Path::new(current_file_path_str)
            .parent()
            .unwrap()
            .join("dummy-workspace"),
    )
    .unwrap();
    std::env::set_current_dir(&root_path).unwrap();

    // git init is a hack for the bump version command to work
    Command::new("git").args(["init"]).output().unwrap();

    let output = assert_cmd::cargo::cargo_bin_cmd!()
        .args(["bump-version", "patch"])
        .unwrap();
    assert!(
        output.status.success(),
        "bump version command should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    defer! {
        fs::remove_dir_all(root_path.join(".git")).unwrap();
        Command::new("git").args(["checkout", "."]).output().unwrap();
    }

    // verfify root/Cargo.toml
    let root_cargo_toml_content = fs::read_to_string(root_path.join("Cargo.toml")).unwrap();
    assert!(
        root_cargo_toml_content.contains(r#"version = "1.2.4""#),
        "workspace.package.version should be bumped to 1.2.4"
    );
    assert!(
        root_cargo_toml_content.contains(r#"a = { path = "a", version = "=1.2.4" }"#),
        "workspace.dependencies.crate-a should be bumped to 1.2.4"
    );
    assert!(
        root_cargo_toml_content.contains(r#"b = { path = "b", version = "=1.2.4" }"#),
        "workspace.dependencies.crate-b should be bumped to 1.2.4"
    );
    assert!(
        root_cargo_toml_content.contains(r#"byte-slice-cast = "=1.2.3""#),
        "non-workspace members' version should not be bumped"
    );
    assert!(
        root_cargo_toml_content.contains(r#"cc = "1.2.3""#),
        "non-workspace members' version should not be bumped"
    );
    assert!(
        root_cargo_toml_content.contains(r#"scopeguard = "1.2.0""#),
        "non-workspace members' version should not be bumped"
    );

    // verify root/Cargo.lock
    let root_cargo_lock_content = fs::read_to_string(root_path.join("Cargo.lock")).unwrap();
    assert!(
        root_cargo_lock_content.contains(r#"version = "1.2.4""#),
        "Cargo.lock should be updated"
    );

    // verify root/d/Cargo.toml
    let d_cargo_toml_content = fs::read_to_string(root_path.join("d/Cargo.toml")).unwrap();
    assert!(
        d_cargo_toml_content.contains(r#"version = "1.2.4""#),
        "d/Cargo.toml should be updated"
    );

    // verify root/d/Cargo.lock
    let d_cargo_lock_content = fs::read_to_string(root_path.join("d/Cargo.lock")).unwrap();
    assert!(
        d_cargo_lock_content.contains(r#"version = "1.2.4""#),
        "d/Cargo.lock should be updated"
    );

    // verify root/sub/Cargo.toml
    let sub_cargo_toml_content = fs::read_to_string(root_path.join("sub/Cargo.toml")).unwrap();
    assert!(
        sub_cargo_toml_content.contains(r#"version = "1.2.4""#),
        "sub/Cargo.toml should be updated"
    );
    assert!(
        sub_cargo_toml_content.contains(r#"c = { path = "c", version = "=1.2.4" }"#),
        "sub/Cargo.toml should be updated"
    );

    // verify root/sub/Cargo.lock
    let sub_cargo_lock_content = fs::read_to_string(root_path.join("sub/Cargo.lock")).unwrap();
    assert!(
        sub_cargo_lock_content.contains(r#"version = "1.2.4""#),
        "sub/Cargo.lock should be updated"
    );
}
