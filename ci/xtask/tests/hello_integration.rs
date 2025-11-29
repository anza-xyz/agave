#[test]
fn test_hello() {
    let output = assert_cmd::cargo::cargo_bin_cmd!()
        .args(["hello", "--verbose"])
        .unwrap();
    assert!(
        output.status.success(),
        "hello command should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // env_logger direct to stderr by default
    let stdout = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("DEBUG MODE"));
    assert!(stdout.contains("Hello!"));
}
