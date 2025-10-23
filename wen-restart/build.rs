fn main() -> Result<(), Box<dyn std::error::Error>> {
    const PROTOC_ENVAR: &str = "PROTOC";
    if std::env::var(PROTOC_ENVAR).is_err() {
        #[cfg(not(windows))]
        {
            // Use vendored protoc to avoid building C++ protobuf via autotools
            let protoc_path = protoc_bin_vendored::protoc_bin_path()?;
            std::env::set_var(PROTOC_ENVAR, protoc_path);
        }
    }

    let proto_base_path = std::path::PathBuf::from("proto");
    let proto = proto_base_path.join("wen_restart.proto");
    println!("cargo:rerun-if-changed={}", proto.display());

    // Generate rust files from protos.
    prost_build::compile_protos(&[proto], &[proto_base_path])?;
    Ok(())
}
