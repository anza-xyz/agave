use std::{fs, path::Path};

#[test]
fn test_publish_order() {
    let current_file_path_str = file!();
    let workspace_path = fs::canonicalize(
        Path::new(current_file_path_str)
            .parent()
            .unwrap()
            .join("dummy-workspace-publish-test"),
    )
    .unwrap();

    let output = assert_cmd::cargo::cargo_bin_cmd!()
        .args([
            "publish",
            "--manifest-path",
            workspace_path.join("Cargo.toml").to_str().unwrap(),
            "order",
            "--format",
            "json",
        ])
        .unwrap();
    assert!(
        output.status.success(),
        "publish order command should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output_json = String::from_utf8_lossy(&output.stdout);
    let output_json: serde_json::Value = serde_json::from_str(&output_json).unwrap();
    assert_eq!(output_json.as_array().unwrap().len(), 4);
    assert_eq!(output_json[0].as_array().unwrap().len(), 1);
    assert_eq!(
        output_json[0][0].get("name").unwrap().as_str().unwrap(),
        "a"
    );
    assert_eq!(output_json[1].as_array().unwrap().len(), 1);
    assert_eq!(
        output_json[1][0].get("name").unwrap().as_str().unwrap(),
        "b"
    );
    assert_eq!(output_json[2].as_array().unwrap().len(), 1);
    assert_eq!(
        output_json[2][0].get("name").unwrap().as_str().unwrap(),
        "c"
    );
    assert_eq!(output_json[3].as_array().unwrap().len(), 1);
    assert_eq!(
        output_json[3][0].get("name").unwrap().as_str().unwrap(),
        "d",
    );
}

#[test]
#[ignore] // requires docker.
fn test_publish_test() {
    let current_file_path_str = file!();
    let workspace_path = fs::canonicalize(
        Path::new(current_file_path_str)
            .parent()
            .unwrap()
            .join("dummy-workspace-publish-test"),
    )
    .unwrap();

    let output = assert_cmd::cargo::cargo_bin_cmd!()
        .args([
            "publish",
            "--manifest-path",
            workspace_path.join("Cargo.toml").to_str().unwrap(),
            "test",
        ])
        .unwrap();
    assert!(
        output.status.success(),
        "publish test command should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // env_logger direct to stderr by default
    let stdout = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("a published"));
    assert!(stdout.contains("b published"));
    assert!(stdout.contains("c published"));
    assert!(stdout.contains("d published"));
}
