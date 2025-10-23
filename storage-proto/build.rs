fn main() -> Result<(), Box<dyn std::error::Error>> {
    const PROTOC_ENVAR: &str = "PROTOC";
    if std::env::var(PROTOC_ENVAR).is_err() {
        // Use vendored protoc to avoid building C++ protobuf via autotools
        let protoc_path = protoc_bin_vendored::protoc_bin_path()?;
        std::env::set_var(PROTOC_ENVAR, protoc_path);
    }

    let proto_base_path = std::path::PathBuf::from("proto");
    let proto_files = [
        "confirmed_block.proto",
        "entries.proto",
        "transaction_by_addr.proto",
    ];
    let mut protos = Vec::new();
    for proto_file in &proto_files {
        let proto = proto_base_path.join(proto_file);
        println!("cargo:rerun-if-changed={}", proto.display());
        protos.push(proto);
    }

    tonic_build::configure()
        .build_client(true)
        .build_server(false)
        .type_attribute(
            "TransactionErrorType",
            "#[cfg_attr(test, derive(enum_iterator::Sequence))]",
        )
        .type_attribute(
            "InstructionErrorType",
            "#[cfg_attr(test, derive(enum_iterator::Sequence))]",
        )
        .compile(&protos, &[proto_base_path])?;

    Ok(())
}
