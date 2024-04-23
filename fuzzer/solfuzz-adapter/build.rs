use std::{io::Result, process::Command};

fn main() -> Result<()> {
    let proto_base_path = std::path::PathBuf::from("proto");

    let protos = &[
        proto_base_path.join("invoke.proto"),
        proto_base_path.join("vm.proto"),
    ];

    protos
        .iter()
        .for_each(|proto| println!("cargo:rerun-if-changed={}", proto.display()));

    prost_build::compile_protos(protos, &[proto_base_path])?;

    println!("cargo:rerun-if-changed=tests/self_test.c");
    Command::new("clang")
        .args(["-o", "target/self_test"])
        .arg("tests/self_test.c")
        .arg("-Werror=all")
        .arg("-ldl")
        .arg("-fsanitize=address,fuzzer-no-link")
        .arg("-fsanitize-coverage=inline-8bit-counters")
        .status()
        .expect("Failed to run C compiler");
    Ok(())
}
