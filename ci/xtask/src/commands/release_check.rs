use {
    anyhow::{Context, Result, anyhow, bail, ensure},
    clap::Args,
    log::info,
    std::{
        env,
        path::{Path, PathBuf},
        process::Command,
    },
};

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

    #[arg(
        long,
        help = "Override the toolchain taken from $rust_nightly, for example: nightly-2026-07-16"
    )]
    pub toolchain: Option<String>,
}

pub fn run(args: CommandArgs) -> Result<()> {
    let CommandArgs {
        profile,
        jobs,
        toolchain,
    } = args;
    let repo_root = repo_root();
    let nightly = match toolchain {
        Some(toolchain) => toolchain,
        None => nightly_toolchain()?,
    };
    let jobs = match jobs {
        Some(jobs) => jobs,
        None => xtask_shared::commands::jobs::jobs()?,
    };

    info!("checking workspace with profile {profile} using {nightly} across {jobs} jobs");

    // rustup, not $CARGO: the latter is a toolchain binary and cannot switch
    // toolchains. RUSTFLAGS stays unset so .cargo/config.toml keeps -Ctarget-cpu.
    let status = Command::new("rustup")
        .current_dir(&repo_root)
        .args(["run", &nightly, "cargo"])
        .args(["check", "--profile", &profile])
        .args(["--workspace", "--all-targets"])
        .args(["--features", "dummy-for-ci-check,frozen-abi"])
        .args(["--jobs", &jobs.to_string()])
        .status()
        .context("failed to run cargo check")?;

    if !status.success() {
        bail!("cargo check failed with {status}");
    }

    Ok(())
}

fn nightly_toolchain() -> Result<String> {
    let toolchain = env::var("rust_nightly").map_err(|_| {
        anyhow!("rust_nightly is unset; run `source ci/rust-version.sh nightly` first")
    })?;
    ensure!(
        toolchain.starts_with("nightly-"),
        "rust_nightly is not a nightly toolchain: {toolchain:?}"
    );

    Ok(toolchain)
}

fn repo_root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    root.canonicalize().unwrap_or(root)
}
