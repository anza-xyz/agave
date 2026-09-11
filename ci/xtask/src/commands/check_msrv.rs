use {
    anyhow::{Context, Result, ensure},
    serde::Deserialize,
    std::fs,
};

#[derive(Deserialize)]
struct ToolchainManifest {
    toolchain: Toolchain,
}

#[derive(Deserialize)]
struct Toolchain {
    channel: String,
}

#[derive(Deserialize)]
struct CargoManifest {
    workspace: Workspace,
}

#[derive(Deserialize)]
struct Workspace {
    package: Package,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Package {
    rust_version: String,
}

pub fn run() -> Result<()> {
    let toolchain =
        fs::read_to_string("rust-toolchain.toml").context("failed to read rust-toolchain.toml")?;
    let manifest = fs::read_to_string("Cargo.toml").context("failed to read Cargo.toml")?;
    check_versions(&toolchain, &manifest)
}

fn check_versions(toolchain: &str, manifest: &str) -> Result<()> {
    let toolchain: ToolchainManifest =
        toml::from_str(toolchain).context("failed to parse rust-toolchain.toml")?;
    let manifest: CargoManifest = toml::from_str(manifest).context("failed to parse Cargo.toml")?;
    let rust_toolchain = toolchain.toolchain.channel;
    let rust_version = manifest.workspace.package.rust_version;
    ensure!(
        rust_toolchain == rust_version,
        "Toolchain {rust_toolchain} and rust-version {rust_version} are out of sync, they must be \
         changed together"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOOLCHAIN: &str = "[toolchain]\nchannel = '1.98.0'";
    const MANIFEST: &str = "[workspace.package]\nrust-version = '1.98.0'";

    #[test]
    fn accepts_matching_versions() {
        check_versions(TOOLCHAIN, MANIFEST).unwrap();
    }

    #[test]
    fn supports_toml_1_1_multiline_inline_tables() {
        // Newlines between inline-table entries and trailing commas require TOML 1.1.
        let toolchain = r#"
            toolchain = {
                channel = "1.98.0",
                components = ["rustfmt", "clippy"],
            }
        "#;
        let manifest = r#"
            [workspace]
            package = {
                rust-version = "1.98.0",
                edition = "2024",
            }

            [workspace.dependencies]
            serde = {
                version = "1.0",
                features = [
                    "derive",
                ],
            }
        "#;

        check_versions(toolchain, manifest).unwrap();

        // A version mismatch must still be detected after parsing TOML 1.1.
        let error = check_versions(toolchain, &manifest.replace("1.98.0", "1.97.0")).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Toolchain 1.98.0 and rust-version 1.97.0 are out of sync, they must be changed \
             together"
        );
    }

    #[test]
    fn rejects_mismatched_versions() {
        let error = check_versions(TOOLCHAIN, &MANIFEST.replace("1.98.0", "1.97.0")).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Toolchain 1.98.0 and rust-version 1.97.0 are out of sync, they must be changed \
             together"
        );
    }

    #[test]
    fn rejects_missing_invalid_and_malformed_fields() {
        for toolchain in [
            "",
            "[toolchain]",
            "[toolchain]\nchannel = 198",
            "[toolchain",
        ] {
            let error = check_versions(toolchain, MANIFEST).unwrap_err();
            assert_eq!(error.to_string(), "failed to parse rust-toolchain.toml");
        }
        for manifest in [
            "",
            "[workspace.package]",
            "[workspace.package]\nrust-version = 198",
            "[workspace",
        ] {
            let error = check_versions(TOOLCHAIN, manifest).unwrap_err();
            assert_eq!(error.to_string(), "failed to parse Cargo.toml");
        }
    }
}
