fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Only compile protobuf files when the fuzz feature is enabled.
    #[cfg(feature = "fuzz")]
    {
        use std::{env, fs, path::PathBuf};

        // Get absolute proto dir from producer
        let proto_dir = PathBuf::from(
            env::var("DEP_PROTOSOL_PROTO_DIR")
                .expect("protosol did not expose PROTO_DIR, did protosol build.rs run first?"),
        );

        println!("cargo:rerun-if-env-changed=DEP_PROTOSOL_PROTO_DIR");
        println!("cargo:rerun-if-changed={}", proto_dir.display());

        // Collect absolute .proto paths
        let mut proto_files = vec![];
        for entry in fs::read_dir(&proto_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) == Some("proto") {
                println!("cargo:rerun-if-changed={}", path.display());
                proto_files.push(path);
            }
        }

        // Ensure deterministic order for rebuilds
        proto_files.sort();

        // Compile protos into Rust
        let out_dir = PathBuf::from(env::var("OUT_DIR")?);
        let mut config = prost_build::Config::new();
        config.out_dir(&out_dir);

        config.compile_protos(
            &proto_files
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>(),
            &[proto_dir.to_str().unwrap()],
        )?;
    }

    Ok(())
}
