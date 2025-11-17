use {
    assert_cmd::assert::Assert,
    predicates::prelude::*,
    std::{env, fs, path::PathBuf, str::FromStr},
};

#[macro_use]
extern crate serial_test;

fn should_install_tools() -> bool {
    let tools_path = env::var("HOME").unwrap();
    let toolchain_path = PathBuf::from(tools_path)
        .join(".cache")
        .join("solana")
        .join("v1.52")
        .join("platform-tools");

    let rust_path = toolchain_path.join("rust");
    let llvm_path = toolchain_path.join("llvm");
    let binaries = rust_path.join("bin");

    let rustc = binaries.join(if cfg!(windows) { "rustc.exe" } else { "rustc" });
    let cargo = binaries.join(if cfg!(windows) { "cargo.exe" } else { "cargo" });

    if !toolchain_path.try_exists().unwrap_or(false)
        || !rust_path.try_exists().unwrap_or(false)
        || !llvm_path.try_exists().unwrap_or(false)
        || !binaries.try_exists().unwrap_or(false)
        || !rustc.try_exists().unwrap_or(false)
        || !cargo.try_exists().unwrap_or(false)
    {
        return true;
    }

    let Ok(folder_metadata) = fs::metadata(rust_path) else {
        return true;
    };
    let Ok(creation_time) = folder_metadata.created() else {
        return true;
    };

    let now = std::time::SystemTime::now();
    let Ok(elapsed_time) = now.duration_since(creation_time) else {
        return true;
    };

    if elapsed_time.as_secs() > 300 {
        return true;
    }

    false
}

fn run_cargo_build(crate_name: &str, extra_args: &[&str], fail: bool) {
    let cwd = env::current_dir().expect("Unable to get current working directory");
    let toml = cwd
        .join("tests")
        .join("crates")
        .join(crate_name)
        .join("Cargo.toml");
    let toml = format!("{}", toml.display());
    let mut args = vec!["-v", "--sbf-sdk", "../sbf", "--manifest-path", &toml];
    if should_install_tools() {
        args.push("--force-tools-install");
    }
    for arg in extra_args {
        args.push(arg);
    }
    if !extra_args.contains(&"--") {
        args.push("--");
    }
    args.push("-vv");
    let mut cmd = assert_cmd::Command::cargo_bin("cargo-build-sbf").unwrap();
    let assert = cmd.env("RUST_LOG", "debug").args(&args).assert();
    let output = assert.get_output();
    eprintln!("Test stdout\n{}\n", String::from_utf8_lossy(&output.stdout));
    eprintln!("Test stderr\n{}\n", String::from_utf8_lossy(&output.stderr));
    if fail {
        assert.failure();
    } else {
        assert.success();
    }
}

fn clean_target(crate_name: &str) {
    let cwd = env::current_dir().expect("Unable to get current working directory");
    let target = cwd
        .join("tests")
        .join("crates")
        .join(crate_name)
        .join("target");
    fs::remove_dir_all(target).expect("Failed to remove target dir");
}

#[test]
#[serial]
fn test_build() {
    run_cargo_build("noop", &[], false);
    clean_target("noop");
}

#[test]
#[serial]
fn test_dump() {
    // This test requires rustfilt.
    assert_cmd::Command::new("cargo")
        .args(["install", "rustfilt"])
        .assert()
        .success();
    run_cargo_build("noop", &["--dump"], false);
    let cwd = env::current_dir().expect("Unable to get current working directory");
    let dump = cwd
        .join("tests")
        .join("crates")
        .join("noop")
        .join("target")
        .join("deploy")
        .join("noop-dump.txt");
    assert!(dump.exists());
    clean_target("noop");
}

#[test]
#[serial]
fn test_out_dir() {
    run_cargo_build("noop", &["--sbf-out-dir", "tmp_out"], false);
    let cwd = env::current_dir().expect("Unable to get current working directory");
    let dir = cwd.join("tmp_out");
    assert!(dir.exists());
    fs::remove_dir_all("tmp_out").expect("Failed to remove tmp_out dir");
    clean_target("noop");
}

#[test]
#[serial]
fn test_target_dir() {
    let target_dir = "./temp-target-dir";
    run_cargo_build("noop", &["--lto", "--", "--target-dir", target_dir], false);
    let cwd = env::current_dir().expect("Unable to get current working directory");
    let normal_target_dir = cwd.join("tests").join("crates").join("noop").join("target");
    assert!(!normal_target_dir.exists());
    let so_file = PathBuf::from_str(target_dir)
        .unwrap()
        .join("deploy")
        .join("noop.so");
    assert!(so_file.exists());
    fs::remove_dir_all(target_dir).expect("Failed to remove custom target dir");
}

#[test]
#[serial]
fn test_target_and_out_dir() {
    let target_dir = "./temp-target-dir";
    run_cargo_build(
        "noop",
        &["--sbf-out-dir", "tmp_out", "--", "--target-dir", target_dir],
        false,
    );
    let cwd = env::current_dir().expect("Unable to get current working directory");
    let dir = cwd.join("tmp_out");
    assert!(dir.exists());
    fs::remove_dir_all("tmp_out").expect("Failed to remove tmp_out dir");
    let normal_target_dir = cwd.join("tests").join("crates").join("noop").join("target");
    assert!(!normal_target_dir.exists());
    fs::remove_dir_all(target_dir).expect("Failed to remove custom target dir");
}

#[test]
#[serial]
fn test_generate_child_script_on_failure() {
    run_cargo_build("fail", &["--generate-child-script-on-failure"], true);
    let cwd = env::current_dir().expect("Unable to get current working directory");
    let scr = cwd
        .join("tests")
        .join("crates")
        .join("fail")
        .join("cargo-build-sbf-child-script-cargo.sh");
    assert!(scr.exists());
    fs::remove_file(scr).expect("Failed to remove script");
    clean_target("fail");
}

fn build_noop_and_readelf(arch: &str) -> Assert {
    run_cargo_build("noop", &["--arch", arch], false);
    let cwd = env::current_dir().expect("Unable to get current working directory");
    let bin = cwd
        .join("tests")
        .join("crates")
        .join("noop")
        .join("target")
        .join("deploy")
        .join("noop.so");
    let bin = bin.to_str().unwrap();
    let root = cwd
        .parent()
        .expect("Unable to get parent directory of current working dir")
        .parent()
        .expect("Unable to get ../.. of current working dir");
    let readelf = root
        .join("platform-tools-sdk")
        .join("sbf")
        .join("dependencies")
        .join("platform-tools")
        .join("llvm")
        .join("bin")
        .join("llvm-readelf");

    assert_cmd::Command::new(readelf).args(["-h", bin]).assert()
}

#[test]
#[serial]
fn test_sbpfv0() {
    let assert_v0 = build_noop_and_readelf("v0");
    assert_v0
        .stdout(predicate::str::contains(
            "Flags:                             0x0",
        ))
        .success();
    clean_target("noop");
}

#[test]
#[serial]
fn test_sbpfv1() {
    let assert_v1 = build_noop_and_readelf("v1");
    assert_v1
        .stdout(predicate::str::contains(
            "Flags:                             0x1",
        ))
        .success();
    clean_target("noop");
}

#[test]
#[serial]
fn test_sbpfv2() {
    let assert_v1 = build_noop_and_readelf("v2");
    assert_v1
        .stdout(predicate::str::contains(
            "Flags:                             0x2",
        ))
        .success();
    clean_target("noop");
}

#[test]
#[serial]
#[ignore]
fn test_sbpfv3() {
    let assert_v1 = build_noop_and_readelf("v3");
    assert_v1
        .stdout(predicate::str::contains(
            "Flags:                             0x3",
        ))
        .success();
    clean_target("noop");
}

#[test]
#[serial]
#[ignore]
fn test_sbpfv4() {
    let assert_v1 = build_noop_and_readelf("v4");
    assert_v1
        .stdout(predicate::str::contains(
            "Flags:                             0x4",
        ))
        .success();
    clean_target("noop");
}

#[test]
#[serial]
fn test_package_metadata_tools_version() {
    run_cargo_build("package-metadata", &[], false);
    clean_target("package-metadata");
}

#[test]
#[serial]
fn test_workspace_metadata_tools_version() {
    run_cargo_build("workspace-metadata", &[], false);
    clean_target("workspace-metadata");
}

#[test]
#[serial]
fn test_corrupted_toolchain() {
    run_cargo_build("noop", &[], false);

    fn assert_failed_command() {
        let cwd = env::current_dir().expect("Unable to get current working directory");
        let toml = cwd
            .join("tests")
            .join("crates")
            .join("noop")
            .join("Cargo.toml");
        let toml = format!("{}", toml.display());
        let args = vec!["--sbf-sdk", "../sbf", "--manifest-path", &toml];

        let mut cmd = assert_cmd::Command::cargo_bin("cargo-build-sbf").unwrap();
        let assert = cmd.env("RUST_LOG", "debug").args(&args).assert();
        let output = assert.get_output();

        assert!(
            String::from_utf8_lossy(&output.stderr).contains("The Solana toolchain is corrupted.")
        );
    }

    let cwd = env::current_dir().expect("Unable to get current working directory");
    let sdk_path = cwd.parent().unwrap().join("sbf");

    let bin_folder = sdk_path
        .join("dependencies")
        .join("platform-tools")
        .join("rust")
        .join("bin");
    fs::rename(bin_folder.join("cargo"), bin_folder.join("cargo_2"))
        .expect("Failed to rename file");

    assert_failed_command();

    fs::rename(bin_folder.join("cargo_2"), bin_folder.join("cargo"))
        .expect("Failed to rename file");
    fs::rename(bin_folder.join("rustc"), bin_folder.join("rustc_2"))
        .expect("Failed to rename file");

    assert_failed_command();

    fs::rename(bin_folder.join("rustc_2"), bin_folder.join("rustc"))
        .expect("Failed to rename file");
    fs::rename(&bin_folder, bin_folder.parent().unwrap().join("bin2"))
        .expect("Failed to rename file");

    assert_failed_command();

    fs::rename(bin_folder.parent().unwrap().join("bin2"), &bin_folder)
        .expect("Failed to rename file");
    let right_rust_folder = bin_folder.parent().unwrap();
    let wrong_rust_folder = right_rust_folder.parent().unwrap().join("rust_2");
    fs::rename(right_rust_folder, &wrong_rust_folder).expect("Failed to rename file");

    assert_failed_command();

    // Revert to the original name, so other tests can run correctly.
    fs::rename(wrong_rust_folder, right_rust_folder).expect("Failed to rename file");
}

#[test]
#[serial]
fn test_alternate_download() {
    let args = [
        "-v",
        "--sbf-sdk",
        "../sbf",
        "--install-only",
        "--force-tools-install",
    ];
    let assert = assert_cmd::Command::cargo_bin("cargo-build-sbf")
        .unwrap()
        .env("RUST_LOG", "debug")
        .args(args)
        .assert();

    assert.success();

    build_noop_and_readelf("v0");
}
