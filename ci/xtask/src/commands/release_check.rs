use {
    anyhow::{Context, Result, anyhow, bail, ensure},
    clap::Args,
    log::info,
    std::{
        env, fs,
        path::{Path, PathBuf},
        process::Command,
    },
};

const BYTES_PER_JOB: u64 = 4 * 1024 * 1024 * 1024;

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
    let jobs = jobs.unwrap_or_else(default_jobs);

    info!("checking workspace with profile {profile} using {nightly} across {jobs} jobs");

    // Goes through rustup rather than $CARGO, which points at a toolchain binary
    // that does not understand +toolchain directives. RUSTFLAGS is left unset on
    // purpose: setting it would replace build.rustflags from .cargo/config.toml,
    // dropping the -Ctarget-cpu release binaries use.
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

// Mirrors ci/common/limit-threads.sh.
fn default_jobs() -> usize {
    if let Some(jobs) = env::var("JOBS").ok().and_then(|v| v.parse().ok()) {
        return jobs;
    }

    let nproc = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let jobs = match mem_total_bytes() {
        Some(bytes) => usize::try_from(bytes / BYTES_PER_JOB)
            .unwrap_or(nproc)
            .min(nproc),
        None => nproc,
    };

    let slots = env::var("CI_HOST_SLOTS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(1);

    if slots > 1 {
        jobs.div_ceil(slots)
    } else {
        jobs
    }
    .max(1)
}

fn mem_total_bytes() -> Option<u64> {
    let meminfo = fs::read_to_string("/proc/meminfo").ok()?;
    let kb: u64 = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(kb.saturating_mul(1024))
}

fn repo_root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    root.canonicalize().unwrap_or(root)
}
