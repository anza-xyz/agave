use std::io::Result;

fn main() -> Result<()> {
    let proto_base_path = std::path::PathBuf::from("protosol/proto");

    let protos = &[
        proto_base_path.join("metadata.proto"),
        proto_base_path.join("context.proto"),
        proto_base_path.join("invoke.proto"),
    ];

    protos
        .iter()
        .for_each(|proto| println!("cargo:rerun-if-changed={}", proto.display()));

    prost_build::Config::new()
        .type_attribute(
            ".",
            "#[cfg_attr(feature = \"serde\", derive(serde::Deserialize, serde::Serialize))]",
        )
        .compile_protos(protos, &[proto_base_path])?;

    Ok(())
}
