use {
    anyhow::{Context, Result, bail, ensure},
    clap::Args,
    log::info,
    serde::Deserialize,
    std::{
        env, fs,
        path::{Path, PathBuf},
        process::Command,
    },
};

#[derive(Deserialize)]
struct CargoManifest {
    workspace: Workspace,
}

#[derive(Deserialize)]
struct Workspace {
    metadata: Metadata,
}

#[derive(Deserialize)]
struct Metadata {
    #[serde(rename = "agave-build-lists")]
    build_lists: BuildLists,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct BuildLists {
    dev: Vec<String>,
    end_user: Vec<String>,
    val_op: Vec<String>,
    dcou: Vec<String>,
    deprecated: Vec<String>,
}

#[derive(Args)]
pub struct CommandArgs {
    #[arg(
        long,
        default_value = "release",
        help = "Cargo profile to check against"
    )]
    pub profile: String,

    #[arg(long, help = "Override the computed job count")]
    pub jobs: Option<usize>,
}

pub fn run(args: CommandArgs) -> Result<()> {
    let CommandArgs { profile, jobs } = args;
    let repo_root = repo_root();
    let jobs = match jobs {
        Some(jobs) => jobs,
        None => xtask_shared::commands::jobs::jobs()?,
    };

    let manifest =
        fs::read_to_string(repo_root.join("Cargo.toml")).context("failed to read Cargo.toml")?;
    let (prod_bins, dcou_bins) = bin_sets(&manifest)?;

    // Mirrors the two builds in scripts/cargo-install-all.sh: dcou bins live in
    // dev-bins so their features do not unify with the production bins.
    info!("checking {profile} production bins across {jobs} jobs: {prod_bins:?}");
    cargo_check(&repo_root, &profile, jobs, &["--workspace"], &prod_bins)?;

    info!("checking {profile} dcou bins across {jobs} jobs: {dcou_bins:?}");
    cargo_check(
        &repo_root,
        &profile,
        jobs,
        &["--manifest-path", "dev-bins/Cargo.toml"],
        &dcou_bins,
    )?;

    Ok(())
}

fn cargo_check(
    repo_root: &Path,
    profile: &str,
    jobs: usize,
    scope: &[&str],
    bins: &[String],
) -> Result<()> {
    // RUSTFLAGS stays unset so .cargo/config.toml keeps -Ctarget-cpu.
    let mut cmd = Command::new(cargo_bin());
    cmd.current_dir(repo_root)
        .args(["check", "--profile", profile])
        .args(scope)
        .args(["--jobs", &jobs.to_string()]);
    for bin in bins {
        cmd.args(["--bin", bin]);
    }

    let status = cmd.status().context("failed to run cargo check")?;
    if !status.success() {
        bail!("cargo check failed with {status}");
    }

    Ok(())
}

/// Production and dcou bins, grouped the same way scripts/cargo-install-all.sh
/// builds them.
fn bin_sets(manifest: &str) -> Result<(Vec<String>, Vec<String>)> {
    let manifest: CargoManifest = toml::from_str(manifest)
        .context("failed to parse [workspace.metadata.agave-build-lists] in Cargo.toml")?;
    let BuildLists {
        dev,
        end_user,
        val_op,
        dcou,
        deprecated,
    } = manifest.workspace.metadata.build_lists;

    let prod_bins: Vec<String> = [deprecated, dev, end_user, val_op].concat();
    ensure!(!prod_bins.is_empty(), "no production bins in Cargo.toml");
    ensure!(!dcou.is_empty(), "no dcou bins in Cargo.toml");

    Ok((prod_bins, dcou))
}

fn cargo_bin() -> PathBuf {
    env::var_os("CARGO")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("cargo"))
}

fn repo_root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    root.canonicalize().unwrap_or(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
        [workspace.metadata.agave-build-lists]
        dev = ["dev-bin"]
        end-user = ["end-user-bin"]
        val-op = ["val-op-bin"]
        dcou = ["dcou-bin"]
        deprecated = ["deprecated-bin"]
        dcou-tainted-packages = ["tainted-package"]
    "#;

    #[test]
    fn groups_bins_like_cargo_install_all() {
        let (prod_bins, dcou_bins) = bin_sets(MANIFEST).unwrap();

        assert_eq!(
            prod_bins,
            ["deprecated-bin", "dev-bin", "end-user-bin", "val-op-bin"]
        );
        assert_eq!(dcou_bins, ["dcou-bin"]);
    }

    #[test]
    fn reads_repo_manifest() {
        let manifest = fs::read_to_string(repo_root().join("Cargo.toml")).unwrap();
        let (prod_bins, dcou_bins) = bin_sets(&manifest).unwrap();

        assert!(prod_bins.iter().any(|bin| bin == "agave-validator"));
        assert!(dcou_bins.iter().any(|bin| bin == "agave-ledger-tool"));
    }

    #[test]
    fn rejects_missing_and_empty_lists() {
        for manifest in [
            "",
            "[workspace.metadata]",
            &MANIFEST.replace("val-op", "valop"),
        ] {
            let error = bin_sets(manifest).unwrap_err();
            assert_eq!(
                error.to_string(),
                "failed to parse [workspace.metadata.agave-build-lists] in Cargo.toml"
            );
        }

        let error = bin_sets(&MANIFEST.replace(r#"["dcou-bin"]"#, "[]")).unwrap_err();
        assert_eq!(error.to_string(), "no dcou bins in Cargo.toml");
    }
}
