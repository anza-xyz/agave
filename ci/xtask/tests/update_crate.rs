use {
    scopeguard::defer,
    serial_test::serial,
    std::{fs, path::Path, process::Command},
};

#[test]
#[serial]
fn test_update_crate() {
    defer! {
        Command::new("git").args(["checkout", "."]).output().unwrap();
    }

    let current_file_path_str = file!();
    let root_path = fs::canonicalize(
        Path::new(current_file_path_str)
            .parent()
            .unwrap()
            .join("dummy-workspace-crates-update"),
    )
    .unwrap();

    let output = assert_cmd::cargo::cargo_bin_cmd!()
        .args([
            "update-crate",
            "--root-path",
            &root_path.display().to_string(),
            "--package",
            "solana-frozen-abi",
            "--from",
            "3.0.0",
            "--to",
            "3.1.0",
            "--exclude-paths",
            "f",
        ])
        .unwrap();
    assert!(
        output.status.success(),
        "update crates command should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // verify Cargo.toml
    let cargo_toml_content = fs::read_to_string(root_path.join("Cargo.toml")).unwrap();
    assert!(
        cargo_toml_content.contains(r#"solana-frozen-abi = "3.1.0""#),
        "solana-frozen-abi should be updated to 3.1.0"
    );

    // verify a/Cargo.toml
    let a_cargo_toml_content = fs::read_to_string(root_path.join("a/Cargo.toml")).unwrap();
    assert!(
        a_cargo_toml_content.contains(r#"solana-frozen-abi = { workspace = true }"#),
        "a/Cargo.toml should not change"
    );

    // verify b/Cargo.toml
    let b_cargo_toml_content = fs::read_to_string(root_path.join("b/Cargo.toml")).unwrap();
    assert!(
        b_cargo_toml_content.contains(r#"solana-frozen-abi = "3.1.0""#),
        "b/Cargo.toml should be updated to 3.1.0"
    );

    // verify c/Cargo.toml
    let c_cargo_toml_content = fs::read_to_string(root_path.join("c/Cargo.toml")).unwrap();
    assert!(
        c_cargo_toml_content.contains(r#"solana-frozen-abi = "=3.1.0""#),
        "c/Cargo.toml should be updated to 3.1.0"
    );

    // verify d/Cargo.toml
    let d_cargo_toml_content = fs::read_to_string(root_path.join("d/Cargo.toml")).unwrap();
    assert!(
        d_cargo_toml_content.contains(r#"solana-frozen-abi = { version = "2.3.0" }"#),
        "d/Cargo.toml should not change"
    );

    // verify e/Cargo.toml
    let e_cargo_toml_content = fs::read_to_string(root_path.join("e/Cargo.toml")).unwrap();
    assert!(
        e_cargo_toml_content.contains(r#"solana-frozen-abi = { version = "=3.1.0" }"#),
        "e/Cargo.toml should be updated to 3.1.0"
    );

    // verify f/Cargo.toml
    let f_cargo_toml_content = fs::read_to_string(root_path.join("sub/f/Cargo.toml")).unwrap();
    assert!(
        f_cargo_toml_content.contains(r#"solana-frozen-abi = "3.0.0""#),
        "f/Cargo.toml should not change because it is excluded"
    );

    // verify g/Cargo.toml
    let g_cargo_toml_content = fs::read_to_string(root_path.join("sub/g/Cargo.toml")).unwrap();
    assert!(
        g_cargo_toml_content.contains(r#"solana-frozen-abi = "=3.1.0""#),
        "g/Cargo.toml should be updated to 3.1.0"
    );
}
