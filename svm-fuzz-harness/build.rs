//! Build script for configuring SVM fuzzing harness.
//!
//! Clones or pulls the latest changes from Firedancer's `protosol` repository
//! and compiles the protobuf bindings into Rust code.

use std::{env::var, io::Result, path::Path, process::Command};

const PROTOSOL_REPOSITORY: &str = "https://github.com/firedancer-io/protosol.git";
// Advance me manually before cutting new patch versions of Agave.
const PROTOSOL_REVISION: &str = "0d695a1dac9c3e68b1ed8b3dc9cb30fe4d0a4b8f";

fn main() -> Result<()> {
    let cargo_out_dir = var("OUT_DIR").expect("Cargo OUT_DIR environment variable not set");
    let proto_out_dir = Path::new(&cargo_out_dir).join("protosol");

    if proto_out_dir.exists() {
        Command::new("git")
            .arg("-C")
            .arg(&proto_out_dir)
            .arg("fetch")
            .status()
            .expect("Failed to execute git pull");
    } else {
        Command::new("git")
            .arg("clone")
            .arg("--depth=1")
            .arg(PROTOSOL_REPOSITORY)
            .arg(&proto_out_dir)
            .status()
            .expect("Failed to execute git clone");
    }
    Command::new("git")
        .arg("-C")
        .arg(&proto_out_dir)
        .arg("checkout")
        .arg(PROTOSOL_REVISION)
        .status()
        .expect("Failed to execute git checkout");

    let proto_base_path = proto_out_dir.join("proto");

    prost_build::compile_protos(
        &[
            proto_base_path.join("invoke.proto"),
            proto_base_path.join("vm.proto"),
            proto_base_path.join("txn.proto"),
            proto_base_path.join("elf.proto"),
            proto_base_path.join("shred.proto"),
            proto_base_path.join("pack.proto"),
        ],
        &[proto_base_path],
    )?;

    Ok(())
}
