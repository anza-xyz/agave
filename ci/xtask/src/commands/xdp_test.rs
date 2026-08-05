use {
    anyhow::{Context, Result, bail, ensure},
    clap::Args,
    log::info,
    serde::Deserialize,
    std::{
        collections::HashMap,
        env,
        ffi::OsString,
        io::{self, Write},
        path::{Path, PathBuf},
        process::{Command, Stdio},
    },
};

const DEFAULT_TESTS: &[&str] = &[
    "netlink_snapshot",
    "route_monitor",
    "router_snapshot",
    "transmitter_smoke",
];

#[derive(Args)]
pub struct CommandArgs {
    #[arg(
        long,
        help = "Build and run the tests with the release-with-debug profile"
    )]
    pub release_with_debug: bool,

    #[arg(
        long,
        help = "Optional command prefix used to run test executables with privileges, for \
                example: sudo -n -E"
    )]
    runner: Option<String>,

    #[arg(long = "test", value_name = "TEST")]
    tests: Vec<String>,

    #[arg(last = true)]
    run_args: Vec<OsString>,
}

pub fn run(args: CommandArgs) -> Result<()> {
    let CommandArgs {
        release_with_debug,
        runner,
        tests,
        run_args,
    } = args;
    let repo_root = repo_root();

    info!("building local xdp tests from {}", repo_root.display());
    if tests.is_empty() {
        let executables = build_tests(&repo_root, DEFAULT_TESTS, release_with_debug)?;
        test_executables(&repo_root, executables, runner, run_args)
    } else {
        let executables = build_tests(&repo_root, &tests, release_with_debug)?;
        test_executables(&repo_root, executables, runner, run_args)
    }
}

fn build_tests<'a, S>(
    repo_root: &Path,
    tests: &'a [S],
    release_with_debug: bool,
) -> Result<Vec<(&'a S, PathBuf)>>
where
    S: AsRef<str>,
{
    let mut cmd = Command::new(cargo_bin());
    cmd.current_dir(repo_root)
        .arg("test")
        .arg("-p")
        .arg("agave-xdp")
        .arg("--features")
        .arg("agave-unstable-api")
        .arg("--no-run")
        .arg("--message-format=json-render-diagnostics")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if release_with_debug {
        cmd.arg("--profile").arg("release-with-debug");
    }
    for test in tests {
        cmd.arg("--test").arg(test.as_ref());
    }

    let output = cmd.output().context("failed to build xdp tests")?;
    io::stderr()
        .write_all(&output.stderr)
        .context("failed to write cargo stderr")?;
    if !output.status.success() {
        bail!("failed to build xdp tests with {}", output.status);
    }

    test_executables_from_cargo_stdout(&output.stdout, tests)
}

#[derive(Deserialize)]
struct CargoMessage {
    reason: String,
    target: Option<CargoTarget>,
    executable: Option<PathBuf>,
}

#[derive(Deserialize)]
struct CargoTarget {
    name: String,
    kind: Vec<String>,
    test: bool,
}

fn test_executables_from_cargo_stdout<'a, S>(
    stdout: &[u8],
    tests: &'a [S],
) -> Result<Vec<(&'a S, PathBuf)>>
where
    S: AsRef<str>,
{
    let mut executables = HashMap::new();
    let stdout = std::str::from_utf8(stdout).context("cargo output is not valid UTF-8")?;
    for line in stdout.lines() {
        let Ok(message) = serde_json::from_str::<CargoMessage>(line) else {
            continue;
        };
        if message.reason != "compiler-artifact" {
            continue;
        }
        let Some(target) = message.target else {
            continue;
        };
        if !target.test || !target.kind.iter().any(|kind| kind == "test") {
            continue;
        }
        let Some(executable) = message.executable else {
            continue;
        };
        executables.insert(target.name, executable);
    }

    tests
        .iter()
        .map(|test| {
            let executable = executables.remove(test.as_ref()).with_context(|| {
                format!("cargo did not report executable for {}", test.as_ref())
            })?;
            Ok((test, executable))
        })
        .collect()
}

fn test_executables<I, S>(
    repo_root: &Path,
    executables: I,
    runner: Option<String>,
    run_args: Vec<OsString>,
) -> Result<()>
where
    I: IntoIterator<Item = (S, PathBuf)>,
    S: AsRef<str>,
{
    for (test, executable) in executables {
        info!("running {} from {}", test.as_ref(), executable.display());
        let mut cmd = command_with_runner(runner.as_deref(), &executable)?;
        cmd.current_dir(repo_root)
            .arg("--include-ignored")
            .arg("--test-threads=1");
        for arg in &run_args {
            cmd.arg(arg);
        }
        let status = cmd
            .status()
            .with_context(|| format!("failed to run {}", test.as_ref()))?;
        ensure!(status.success(), "{} failed with {status}", test.as_ref());
    }

    Ok(())
}

fn repo_root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    root.canonicalize().unwrap_or(root)
}

fn command_with_runner(runner: Option<&str>, program: &Path) -> Result<Command> {
    let Some(runner) = runner else {
        return Ok(Command::new(program));
    };
    let mut parts = runner.split_whitespace();
    let Some(runner_program) = parts.next() else {
        bail!("runner cannot be empty");
    };
    let mut cmd = Command::new(runner_program);
    cmd.args(parts).arg(program);
    Ok(cmd)
}

fn cargo_bin() -> PathBuf {
    env::var_os("CARGO")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("cargo"))
}
