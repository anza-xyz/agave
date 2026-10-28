fn main() -> Result<(), std::io::Error> {
    const PROTOC_ENVAR: &str = "PROTOC";
    // Safety: env is checked and updated before any threads might exist
    if std::env::var(PROTOC_ENVAR).is_err() {
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var(PROTOC_ENVAR, protobuf_src::protoc())
        }
    }

    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    let out_dir = manifest_dir.join("../proto");
    let googleapis = manifest_dir.join("googleapis");

    println!("Google API directory: {}", googleapis.display());
    println!("output directory: {}", out_dir.display());

    let mut config = tonic_prost_build::Config::new();
    config.disable_comments(["."]);

    tonic_prost_build::configure()
        .build_client(true)
        .build_server(false)
        .out_dir(&out_dir)
        .type_attribute("ProtoRows", "#[allow(dead_code)]")
        .compile_with_config(
            config,
            &[googleapis.join("google/bigtable/v2/bigtable.proto")],
            &[googleapis],
        )
}
