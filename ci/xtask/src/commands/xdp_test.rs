use {
    anyhow::{Context, Result, anyhow},
    clap::{Args, Subcommand},
    log::info,
    std::{
        env,
        ffi::OsString,
        path::{Path, PathBuf},
        process::Command,
    },
};

#[derive(Args, Debug)]
pub struct CommandArgs {
    /// Build and run the XDP integration tests with Cargo's release profile.
    #[arg(long, global = true)]
    release: bool,

    #[command(subcommand)]
    environment: Environment,
}

#[derive(Subcommand, Debug)]
enum Environment {
    #[command(about = "Run XDP integration tests locally")]
    Local(LocalArgs),
}

#[derive(Args, Debug)]
struct LocalArgs {
    /// Wrapper command used to acquire privileges. Use `none` when already running as root.
    #[arg(long, default_value = "sudo -E")]
    runner: String,

    /// Specific XDP integration tests to run. Defaults to the example tests.
    #[arg(long = "test", value_name = "NAME")]
    tests: Vec<String>,

    /// Extra arguments passed directly to the Rust test binary after `--`.
    #[arg(last = true)]
    run_args: Vec<OsString>,
}

pub fn run(args: CommandArgs) -> Result<()> {
    let release = args.release;
    match args.environment {
        Environment::Local(args) => run_local(args, release),
    }
}

fn run_local(args: LocalArgs, release: bool) -> Result<()> {
    let cargo = cargo_program();
    let repo_root = repo_root();
    let tests = if args.tests.is_empty() {
        vec![
            String::from("netlink_snapshot"),
            String::from("route_monitor"),
        ]
    } else {
        args.tests
    };

    let mut cmd = command_with_runner(&args.runner, &cargo)?;
    forward_toolchain_env(&mut cmd, &cargo);
    cmd.current_dir(&repo_root);
    cmd.args([
        "test",
        "-p",
        "agave-xdp",
        "--features",
        "agave-unstable-api",
    ]);
    if release {
        cmd.arg("--release");
    }
    for test in &tests {
        cmd.args(["--test", test]);
    }
    cmd.arg("--");
    cmd.arg("--test-threads=1");
    cmd.args(args.run_args);

    info!("running XDP integration tests: {cmd:?}");

    let status = cmd
        .status()
        .with_context(|| format!("failed to run {cmd:?}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("{cmd:?} failed: {status}"))
    }
}

fn command_with_runner(runner: &str, cargo: &PathBuf) -> Result<Command> {
    if runner == "none" {
        return Ok(Command::new(cargo));
    }

    let mut parts = runner.split_whitespace();
    let program = parts
        .next()
        .ok_or_else(|| anyhow!("runner must not be empty"))?;
    let mut cmd = Command::new(program);
    cmd.args(parts);
    cmd.arg(cargo);
    Ok(cmd)
}

fn cargo_program() -> PathBuf {
    env::var_os("CARGO")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("cargo"))
}

fn forward_toolchain_env(cmd: &mut Command, cargo: &Path) {
    for key in [
        "HOME",
        "PATH",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "RUSTUP_TOOLCHAIN",
    ] {
        if let Some(value) = env::var_os(key) {
            cmd.env(key, value);
        }
    }

    if let Some(tool_dir) = cargo.parent() {
        cmd.env(
            "RUSTC",
            env::var_os("RUSTC").unwrap_or_else(|| tool_dir.join("rustc").into_os_string()),
        );
        cmd.env(
            "RUSTDOC",
            env::var_os("RUSTDOC").unwrap_or_else(|| tool_dir.join("rustdoc").into_os_string()),
        );
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("ci/xtask should live two levels below the repo root")
        .to_path_buf()
}
