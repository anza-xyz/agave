use {
    anyhow::{Context, Result, anyhow, bail, ensure},
    ar::Archive as ArArchive,
    cargo_metadata::{Artifact, Message},
    clap::{Args, Subcommand, ValueEnum},
    flate2::read::GzDecoder,
    log::info,
    regex::Regex,
    std::{
        cmp::Ordering,
        collections::{BTreeMap, BTreeSet},
        env, fs,
        io::{BufReader, Read, Write},
        path::{Path, PathBuf},
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    },
    tar::Archive as TarArchive,
    tempfile::TempDir,
    walkdir::WalkDir,
    xz2::read::XzDecoder,
};

const DEFAULT_TESTS: &[&str] = &[
    "netlink_snapshot",
    "route_monitor",
    "router_snapshot",
    "transmitter_smoke",
    "zero_copy_hardware",
];
const DEFAULT_PR_KERNEL_VERSIONS: &[&str] = &["6.8", "6.17", "7.0"];
const DEFAULT_NIGHTLY_KERNEL_VERSIONS: &[&str] = &["6.8", "6.17", "7.0"];

#[derive(Args)]
pub struct CommandArgs {
    #[arg(long, help = "Build and run the tests with the release profile")]
    pub release: bool,

    #[command(subcommand)]
    environment: Environment,
}

#[derive(Subcommand)]
enum Environment {
    #[command(about = "Run the tests directly on the host")]
    Local(LocalArgs),
    #[command(about = "Download and cache Debian kernel archives for VM tests")]
    FetchKernels(FetchKernelsArgs),
    #[command(
        about = "Boot a QEMU guest and run the tests inside it",
        long_about = "Boot a QEMU guest and run the tests inside it.\n\nRequires \
                      `qemu-system-x86_64` to be installed and available on PATH, or pass `--qemu \
                      /full/path/to/qemu-system-x86_64`."
    )]
    Vm(VmArgs),
}

#[derive(Args)]
pub struct LocalArgs {
    #[arg(
        long,
        default_value = "none",
        help = "Optional command prefix used to run cargo with privileges, for example: sudo -n -E"
    )]
    runner: String,

    #[arg(long = "test", value_name = "TEST")]
    tests: Vec<String>,

    #[arg(last = true)]
    run_args: Vec<std::ffi::OsString>,
}

#[derive(Args)]
pub struct VmArgs {
    #[arg(long = "kernel-archive", value_name = "DEB")]
    kernel_archives: Vec<PathBuf>,

    #[arg(long = "kernel-tree", value_name = "DIR")]
    kernel_trees: Vec<PathBuf>,

    #[arg(long = "kernel-set", value_enum, value_name = "SET")]
    kernel_sets: Vec<KernelSet>,

    #[command(flatten)]
    kernel_versions: KernelVersionArgs,

    #[arg(long, default_value_os_t = default_cache_dir())]
    cache_dir: PathBuf,

    #[arg(long = "test", value_name = "TEST")]
    tests: Vec<String>,

    #[arg(
        long,
        default_value_os_t = default_qemu(),
        help = "QEMU binary to use. Defaults to `qemu-system-x86_64`, which must be installed and on PATH."
    )]
    qemu: PathBuf,

    #[arg(long, default_value_t = 180)]
    timeout_secs: u64,

    #[arg(last = true)]
    run_args: Vec<std::ffi::OsString>,
}

#[derive(Args)]
pub struct FetchKernelsArgs {
    #[arg(long = "kernel-set", value_enum, value_name = "SET", required = true)]
    kernel_sets: Vec<KernelSet>,

    #[command(flatten)]
    kernel_versions: KernelVersionArgs,

    #[arg(long, default_value_os_t = default_cache_dir())]
    cache_dir: PathBuf,
}

#[derive(Args, Clone, Debug, Default)]
struct KernelVersionArgs {
    #[arg(
        long = "pr-kernel-version",
        value_name = "VERSION",
        help = "Override the default PR kernel versions. Repeat to provide multiple versions."
    )]
    pr_kernel_versions: Vec<String>,

    #[arg(
        long = "nightly-kernel-version",
        value_name = "VERSION",
        help = "Override the default nightly kernel versions. Repeat to provide multiple versions."
    )]
    nightly_kernel_versions: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, ValueEnum)]
enum KernelSet {
    Pr,
    Nightly,
}

enum KernelSource {
    Archive(PathBuf),
    ArchiveBundle {
        label: String,
        archives: Vec<PathBuf>,
    },
    Tree(PathBuf),
}

struct PreparedKernel {
    label: String,
    kernel_image: PathBuf,
    modules_dir: Option<PathBuf>,
    release: Option<String>,
}

pub fn run(args: CommandArgs) -> Result<()> {
    match args.environment {
        Environment::Local(local) => run_local(local, args.release),
        Environment::FetchKernels(fetch) => fetch_kernels(fetch),
        Environment::Vm(vm) => run_vm(vm, args.release),
    }
}

fn run_local(args: LocalArgs, release: bool) -> Result<()> {
    if args.runner == "none" && unsafe { libc::geteuid() } != 0 {
        bail!("xdp-test local needs root or a runner such as `--runner \"sudo -n -E\"`");
    }

    let repo_root = repo_root()?;
    let tests = test_selection(&args.tests);
    let mut cmd = command_with_runner(&args.runner, &cargo_bin())?;
    cmd.current_dir(&repo_root);
    forward_cargo_env(&mut cmd);
    cmd.arg("test")
        .arg("-p")
        .arg("agave-xdp")
        .arg("--features")
        .arg("agave-unstable-api");
    if release {
        cmd.arg("--release");
    }
    for test in &tests {
        cmd.arg("--test").arg(test);
    }
    cmd.arg("--")
        .arg("--include-ignored")
        .arg("--test-threads=1");
    for arg in &args.run_args {
        cmd.arg(arg);
    }

    info!("running local xdp tests from {}", repo_root.display());
    let status = cmd.status().context("failed to run cargo test")?;
    ensure!(status.success(), "xdp tests failed with {status}");
    Ok(())
}

fn run_vm(args: VmArgs, release: bool) -> Result<()> {
    ensure!(
        !(args.kernel_archives.is_empty()
            && args.kernel_trees.is_empty()
            && args.kernel_sets.is_empty()),
        "pass at least one --kernel-archive, --kernel-tree, or --kernel-set",
    );

    let repo_root = repo_root()?;
    let tests = test_selection(&args.tests);
    let guest_run_args = guest_run_args(&args.run_args)?;
    let test_binaries = build_test_binaries(&repo_root, &tests, release)?;
    let guest_init = build_guest_init(&repo_root, release)?;
    let cache_dir = resolve_user_path(&args.cache_dir)?;
    fs::create_dir_all(&cache_dir)
        .with_context(|| format!("failed to create {}", cache_dir.display()))?;

    let mut kernels = Vec::new();
    for archive in args.kernel_archives {
        kernels.push(KernelSource::Archive(resolve_user_path(&archive)?));
    }
    for tree in args.kernel_trees {
        kernels.push(KernelSource::Tree(resolve_user_path(&tree)?));
    }
    let downloaded_sources =
        resolve_kernel_set_sources(&cache_dir, &args.kernel_sets, &args.kernel_versions)?;
    kernels.extend(downloaded_sources);

    let qemu = resolve_command_path(&args.qemu)?;
    let timeout = Duration::from_secs(args.timeout_secs);
    let mut seen = BTreeSet::new();
    let mut failures = Vec::new();
    let mut passes = Vec::new();

    for kernel_source in kernels {
        let kernel_key = kernel_source_label(&kernel_source);
        if !seen.insert(kernel_key.clone()) {
            continue;
        }
        match prepare_kernel(&kernel_source, &cache_dir) {
            Ok(kernel) => {
                if let Err(err) = run_vm_for_kernel(
                    &kernel,
                    &guest_init,
                    &test_binaries,
                    &guest_run_args,
                    &qemu,
                    timeout,
                ) {
                    failures.push(format!("{}: {err:#}", kernel.label));
                    eprintln!("vm xdp tests: FAIL {}", kernel.label);
                } else {
                    println!("vm xdp tests: PASS {}", kernel.label);
                    passes.push(kernel.label);
                }
            }
            Err(err) => failures.push(format!("{}: {err:#}", kernel_source_label(&kernel_source))),
        }
    }

    if !passes.is_empty() {
        println!("vm xdp tests passed on:");
        for label in &passes {
            println!("  {label}");
        }
    }

    if !failures.is_empty() {
        bail!("vm xdp tests failed:\n{}", failures.join("\n"));
    }

    Ok(())
}

fn fetch_kernels(args: FetchKernelsArgs) -> Result<()> {
    let cache_dir = resolve_user_path(&args.cache_dir)?;
    fs::create_dir_all(&cache_dir)
        .with_context(|| format!("failed to create {}", cache_dir.display()))?;
    let sources = resolve_kernel_set_sources(&cache_dir, &args.kernel_sets, &args.kernel_versions)?;
    for source in sources {
        println!("{}", kernel_source_label(&source));
    }
    Ok(())
}

fn test_selection(selected: &[String]) -> Vec<String> {
    if selected.is_empty() {
        return DEFAULT_TESTS
            .iter()
            .map(|test| (*test).to_string())
            .collect();
    }
    selected.to_vec()
}

fn guest_run_args(run_args: &[std::ffi::OsString]) -> Result<Vec<String>> {
    let mut args = vec![
        "--include-ignored".to_string(),
        "--test-threads=1".to_string(),
    ];
    for arg in run_args {
        args.push(
            arg.to_str()
                .ok_or_else(|| anyhow!("non-utf8 test argument is not supported in vm mode"))?
                .to_string(),
        );
    }
    Ok(args)
}

fn resolve_kernel_set_sources(
    cache_dir: &Path,
    kernel_sets: &[KernelSet],
    kernel_version_args: &KernelVersionArgs,
) -> Result<Vec<KernelSource>> {
    if kernel_sets.is_empty() {
        ensure!(
            kernel_version_args.pr_kernel_versions.is_empty()
                && kernel_version_args.nightly_kernel_versions.is_empty(),
            "kernel version overrides require a matching --kernel-set",
        );
        return Ok(Vec::new());
    }

    validate_kernel_version_args(kernel_sets, kernel_version_args)?;

    let download_dir = cache_dir.join("debian-kernels").join("amd64");
    fs::create_dir_all(&download_dir)
        .with_context(|| format!("failed to create {}", download_dir.display()))?;

    let versions = requested_kernel_versions(kernel_sets, kernel_version_args);
    let index = fetch_debian_kernel_index()?;
    let mut sources = Vec::new();
    for version in versions {
        let source = resolve_debian_kernel_source(&index, &download_dir, "amd64", &version)
            .with_context(|| format!("failed to resolve a Debian kernel for version {version}"))?;
        sources.push(source);
    }

    Ok(sources)
}

fn validate_kernel_version_args(
    kernel_sets: &[KernelSet],
    kernel_version_args: &KernelVersionArgs,
) -> Result<()> {
    let requested = kernel_sets.iter().copied().collect::<BTreeSet<_>>();
    ensure!(
        kernel_version_args.pr_kernel_versions.is_empty() || requested.contains(&KernelSet::Pr),
        "--pr-kernel-version requires --kernel-set pr",
    );
    ensure!(
        kernel_version_args.nightly_kernel_versions.is_empty()
            || requested.contains(&KernelSet::Nightly),
        "--nightly-kernel-version requires --kernel-set nightly",
    );
    Ok(())
}

fn requested_kernel_versions(
    kernel_sets: &[KernelSet],
    kernel_version_args: &KernelVersionArgs,
) -> BTreeSet<String> {
    let mut versions = BTreeSet::new();
    for kernel_set in kernel_sets {
        for version in kernel_versions(*kernel_set, kernel_version_args) {
            versions.insert(version.to_string());
        }
    }
    versions
}

fn kernel_versions(kernel_set: KernelSet, kernel_version_args: &KernelVersionArgs) -> Vec<&str> {
    match kernel_set {
        KernelSet::Pr if !kernel_version_args.pr_kernel_versions.is_empty() => kernel_version_args
            .pr_kernel_versions
            .iter()
            .map(String::as_str)
            .collect(),
        KernelSet::Pr => DEFAULT_PR_KERNEL_VERSIONS.to_vec(),
        KernelSet::Nightly if !kernel_version_args.nightly_kernel_versions.is_empty() => {
            kernel_version_args
                .nightly_kernel_versions
                .iter()
                .map(String::as_str)
                .collect()
        }
        KernelSet::Nightly => DEFAULT_NIGHTLY_KERNEL_VERSIONS.to_vec(),
    }
}

fn fetch_debian_kernel_index() -> Result<Vec<String>> {
    const MIRRORS: &[&str] = &[
        "https://mirrors.wikimedia.org/debian/pool/main/l/linux/",
        "https://mirrors.wikimedia.org/debian/pool/main/l/linux-signed-amd64/",
    ];

    let mut urls = Vec::new();
    for mirror in MIRRORS {
        urls.extend(fetch_deb_links(mirror)?);
    }
    ensure!(
        !urls.is_empty(),
        "no .deb links found in Debian kernel pools"
    );
    Ok(urls)
}

fn fetch_deb_links(index_url: &str) -> Result<Vec<String>> {
    let href = Regex::new(r#"href="([^"]+\.deb)""#).context("invalid href regex")?;
    let html = fetch_url(index_url)?;
    Ok(href
        .captures_iter(&html)
        .filter_map(|captures| captures.get(1).map(|path| path.as_str()))
        .map(|path| absolute_url(index_url, path))
        .collect())
}

fn fetch_url(url: &str) -> Result<String> {
    let output = Command::new("curl")
        .args(["-fsSL", url])
        .output()
        .with_context(|| format!("failed to fetch {url}"))?;
    ensure!(
        output.status.success(),
        "curl failed for {url} with {}",
        output.status
    );
    String::from_utf8(output.stdout).with_context(|| format!("URL is not valid UTF-8: {url}"))
}

fn absolute_url(base: &str, path: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
        return path.to_string();
    }

    if path.starts_with('/') {
        if let Some(scheme_end) = base.find("://") {
            let origin_start = scheme_end + 3;
            let origin_end = base[origin_start..]
                .find('/')
                .map(|index| origin_start + index)
                .unwrap_or(base.len());
            return format!("{}{}", &base[..origin_end], path);
        }
    }

    if base.ends_with('/') {
        format!("{base}{path}")
    } else if let Some((directory, _)) = base.rsplit_once('/') {
        format!("{directory}/{path}")
    } else {
        path.to_string()
    }
}

fn resolve_debian_kernel_source(
    index: &[String],
    download_dir: &Path,
    arch: &str,
    version: &str,
) -> Result<KernelSource> {
    resolve_debian_kernel_source_from_index(index, download_dir, arch, version).or_else(|err| {
        let snapshot_index = fetch_debian_snapshot_kernel_index(version)
            .with_context(|| format!("failed to fetch Debian snapshot index for {version}"))?;
        resolve_debian_kernel_source_from_index(&snapshot_index, download_dir, arch, version)
            .with_context(|| format!("current Debian kernel pools did not match: {err}"))
    })
}

fn resolve_debian_kernel_source_from_index(
    index: &[String],
    download_dir: &Path,
    arch: &str,
    version: &str,
) -> Result<KernelSource> {
    if let Ok(url) = select_monolithic_kernel_url(index, arch, version) {
        return Ok(KernelSource::Archive(download_kernel_url(
            &url,
            download_dir,
        )?));
    }

    let binary_url = select_split_binary_kernel_url(index, arch, version)?;
    let release = split_kernel_release(&binary_url)?;
    let modules_url = select_split_modules_kernel_url(index, &release)?;
    let binary = download_kernel_url(&binary_url, download_dir)?;
    let modules = download_kernel_url(&modules_url, download_dir)?;
    Ok(KernelSource::ArchiveBundle {
        label: release,
        archives: vec![binary, modules],
    })
}

fn fetch_debian_snapshot_kernel_index(version: &str) -> Result<Vec<String>> {
    const SNAPSHOT_PACKAGES: &[&str] = &[
        "https://snapshot.debian.org/package/linux/",
        "https://snapshot.debian.org/package/linux-signed-amd64/",
    ];

    let version_prefix = format!("{version}.");
    let href = Regex::new(r#"href="([^"]+/)""#).context("invalid href regex")?;
    let mut urls = Vec::new();
    for package_url in SNAPSHOT_PACKAGES {
        let html = fetch_url(package_url)?;
        for path in href
            .captures_iter(&html)
            .filter_map(|captures| captures.get(1).map(|path| path.as_str()))
            .filter(|path| snapshot_version_path_matches(path, &version_prefix))
        {
            let version_url = absolute_url(package_url, path);
            if let Ok(links) = fetch_deb_links(&version_url) {
                urls.extend(links);
            }
        }
    }

    ensure!(
        !urls.is_empty(),
        "no .deb links found in Debian snapshot for kernel version {version}"
    );
    Ok(urls)
}

fn snapshot_version_path_matches(path: &str, version_prefix: &str) -> bool {
    path.trim_end_matches('/')
        .replace("%2B", "+")
        .replace("%7E", "~")
        .starts_with(version_prefix)
}

fn download_kernel_url(url: &str, download_dir: &Path) -> Result<PathBuf> {
    let file_name = url
        .rsplit('/')
        .next()
        .ok_or_else(|| anyhow!("unexpected kernel url: {url}"))?;
    let destination = download_dir.join(file_name);
    download_if_missing(url, &destination)?;
    Ok(destination)
}

fn select_monolithic_kernel_url(index: &[String], arch: &str, version: &str) -> Result<String> {
    let version = regex::escape(version);
    let arch = regex::escape(arch);
    let pattern = format!(
        r"linux-image-{version}\.[0-9]+(-[0-9]+)?(\+bpo|\+deb[0-9]+(\.[0-9]+)?)?-cloud-{arch}-unsigned_.*\.deb$"
    );
    select_latest_matching_url(index, &pattern)
}

fn select_split_binary_kernel_url(index: &[String], arch: &str, version: &str) -> Result<String> {
    let version = regex::escape(version);
    let arch = regex::escape(arch);
    let pattern = format!(
        r"linux-binary-{version}\.[0-9]+(-[0-9]+)?(\+bpo|\+deb[0-9]+(\.[0-9]+)?)?-cloud-{arch}_.*\.deb$"
    );
    select_latest_matching_url(index, &pattern)
}

fn select_split_modules_kernel_url(index: &[String], release: &str) -> Result<String> {
    let release = regex::escape(release);
    let pattern = format!(r"linux-modules-{release}_.*\.deb$");
    select_latest_matching_url(index, &pattern)
}

fn select_latest_matching_url(index: &[String], pattern: &str) -> Result<String> {
    let regex = Regex::new(pattern).with_context(|| format!("invalid kernel regex: {pattern}"))?;
    index
        .iter()
        .filter(|url| regex.is_match(url))
        .max_by(|left, right| version_cmp(left, right))
        .cloned()
        .ok_or_else(|| anyhow!("no kernel matched {pattern}"))
}

fn split_kernel_release(url: &str) -> Result<String> {
    let file_name = url
        .rsplit('/')
        .next()
        .ok_or_else(|| anyhow!("unexpected kernel url: {url}"))?;
    file_name
        .strip_prefix("linux-binary-")
        .and_then(|name| name.split_once('_').map(|(release, _)| release.to_string()))
        .ok_or_else(|| anyhow!("unexpected split kernel package name: {file_name}"))
}

fn download_if_missing(url: &str, destination: &Path) -> Result<()> {
    if destination.is_file() {
        info!("using cached kernel {}", destination.display());
        return Ok(());
    }

    let part = destination.with_extension("part");
    if part.exists() {
        fs::remove_file(&part).with_context(|| format!("failed to remove {}", part.display()))?;
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    info!("downloading kernel {url}");
    let status = Command::new("curl")
        .args(["-fsSL", "-o"])
        .arg(&part)
        .arg(url)
        .status()
        .with_context(|| format!("failed to download {url}"))?;
    ensure!(status.success(), "curl failed for {url} with {status}");

    fs::rename(&part, destination).with_context(|| {
        format!(
            "failed to move {} into {}",
            part.display(),
            destination.display(),
        )
    })?;
    Ok(())
}

fn version_cmp(left: &str, right: &str) -> Ordering {
    let mut left = left.as_bytes();
    let mut right = right.as_bytes();

    while !left.is_empty() && !right.is_empty() {
        let left_is_digit = left[0].is_ascii_digit();
        let right_is_digit = right[0].is_ascii_digit();
        if left_is_digit && right_is_digit {
            let left_len = left.iter().take_while(|byte| byte.is_ascii_digit()).count();
            let right_len = right
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            let left_chunk = &left[..left_len];
            let right_chunk = &right[..right_len];

            let left_trimmed = trim_leading_zeroes(left_chunk);
            let right_trimmed = trim_leading_zeroes(right_chunk);
            match left_trimmed.len().cmp(&right_trimmed.len()) {
                Ordering::Equal => match left_trimmed.cmp(right_trimmed) {
                    Ordering::Equal => {}
                    ordering => return ordering,
                },
                ordering => return ordering,
            }

            left = &left[left_len..];
            right = &right[right_len..];
            continue;
        }

        if left_is_digit != right_is_digit {
            return if left_is_digit {
                Ordering::Greater
            } else {
                Ordering::Less
            };
        }

        let left_len = left
            .iter()
            .take_while(|byte| !byte.is_ascii_digit())
            .count();
        let right_len = right
            .iter()
            .take_while(|byte| !byte.is_ascii_digit())
            .count();
        let left_chunk = &left[..left_len];
        let right_chunk = &right[..right_len];
        match left_chunk.cmp(right_chunk) {
            Ordering::Equal => {}
            ordering => return ordering,
        }

        left = &left[left_len..];
        right = &right[right_len..];
    }

    left.len().cmp(&right.len())
}

fn trim_leading_zeroes(bytes: &[u8]) -> &[u8] {
    let trimmed = bytes
        .iter()
        .position(|byte| *byte != b'0')
        .map(|index| &bytes[index..]);
    match trimmed {
        Some(bytes) if !bytes.is_empty() => bytes,
        _ => b"0",
    }
}

fn build_test_binaries(
    repo_root: &Path,
    tests: &[String],
    release: bool,
) -> Result<BTreeMap<String, PathBuf>> {
    let mut cmd = Command::new(cargo_bin());
    cmd.current_dir(repo_root);
    forward_cargo_env(&mut cmd);
    cmd.arg("test")
        .arg("-p")
        .arg("agave-xdp")
        .arg("--features")
        .arg("agave-unstable-api");
    if release {
        cmd.arg("--release");
    }
    cmd.arg("--no-run")
        .arg("--message-format=json-render-diagnostics");
    for test in tests {
        cmd.arg("--test").arg(test);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::inherit());

    let mut child = cmd.spawn().context("failed to spawn cargo test --no-run")?;
    let stdout = child
        .stdout
        .take()
        .context("failed to capture cargo metadata output")?;
    let reader = BufReader::new(stdout);
    let mut binaries = BTreeMap::new();
    for message in Message::parse_stream(reader) {
        let message = message.context("failed to parse cargo message stream")?;
        if let Message::CompilerArtifact(artifact) = message {
            capture_test_binary(&artifact, tests, &mut binaries);
        }
    }

    let status = child.wait().context("failed to wait for cargo")?;
    ensure!(status.success(), "cargo test --no-run failed with {status}");

    let missing = tests
        .iter()
        .filter(|test| !binaries.contains_key(*test))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!("failed to locate test binaries for {}", missing.join(", "));
    }

    Ok(binaries)
}

fn capture_test_binary(
    artifact: &Artifact,
    tests: &[String],
    binaries: &mut BTreeMap<String, PathBuf>,
) {
    if !tests.iter().any(|test| test == &artifact.target.name) {
        return;
    }
    if let Some(executable) = &artifact.executable {
        binaries.insert(
            artifact.target.name.clone(),
            executable.clone().into_std_path_buf(),
        );
    }
}

fn build_guest_init(repo_root: &Path, release: bool) -> Result<PathBuf> {
    let manifest = repo_root.join("ci/xtask/Cargo.toml");
    let target_dir = repo_root.join("target/xdp-vm-init");
    let mut cmd = Command::new(cargo_bin());
    cmd.current_dir(repo_root);
    forward_cargo_env(&mut cmd);
    cmd.env("CARGO_TARGET_DIR", &target_dir)
        .arg("build")
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--bin")
        .arg("xdp_vm_init");
    if release {
        cmd.arg("--release");
    }

    let status = cmd.status().context("failed to build xdp_vm_init")?;
    ensure!(
        status.success(),
        "building xdp_vm_init failed with {status}"
    );

    let profile = if release { "release" } else { "debug" };
    let binary = target_dir.join(profile).join("xdp_vm_init");
    ensure!(
        binary.is_file(),
        "guest init binary missing at {}",
        binary.display()
    );
    Ok(binary)
}

fn run_vm_for_kernel(
    kernel: &PreparedKernel,
    guest_init: &Path,
    test_binaries: &BTreeMap<String, PathBuf>,
    run_args: &[String],
    qemu: &Path,
    timeout: Duration,
) -> Result<()> {
    let temp_dir = TempDir::new().context("failed to create vm staging dir")?;
    let guest_root = temp_dir.path().join("guest");
    stage_guest_tree(&guest_root, guest_init, test_binaries, kernel)?;
    let initramfs = temp_dir.path().join("initramfs.cpio");
    build_initramfs(&guest_root, &initramfs)?;
    run_qemu_vm(kernel, &initramfs, run_args, qemu, timeout)
}

fn prepare_kernel(source: &KernelSource, cache_dir: &Path) -> Result<PreparedKernel> {
    match source {
        KernelSource::Archive(archive) => {
            let cache_entry = cache_dir.join(archive_stem(archive));
            let marker = cache_entry.join(".prepared");
            if !marker.is_file() {
                if cache_entry.exists() {
                    fs::remove_dir_all(&cache_entry)
                        .with_context(|| format!("failed to clear {}", cache_entry.display()))?;
                }
                fs::create_dir_all(&cache_entry)
                    .with_context(|| format!("failed to create {}", cache_entry.display()))?;
                extract_kernel_archive(archive, &cache_entry)?;
                fs::write(&marker, b"ok")
                    .with_context(|| format!("failed to write {}", marker.display()))?;
            }
            inspect_kernel_tree(&cache_entry, archive.display().to_string())
        }
        KernelSource::ArchiveBundle { label, archives } => {
            let cache_entry = cache_dir
                .join("bundled-kernels")
                .join(safe_cache_name(label));
            let marker = cache_entry.join(".prepared");
            if !marker.is_file() {
                if cache_entry.exists() {
                    fs::remove_dir_all(&cache_entry)
                        .with_context(|| format!("failed to clear {}", cache_entry.display()))?;
                }
                fs::create_dir_all(&cache_entry)
                    .with_context(|| format!("failed to create {}", cache_entry.display()))?;
                for archive in archives {
                    extract_kernel_archive(archive, &cache_entry)?;
                }
                fs::write(&marker, b"ok")
                    .with_context(|| format!("failed to write {}", marker.display()))?;
            }
            inspect_kernel_tree(&cache_entry, label.clone())
        }
        KernelSource::Tree(tree) => inspect_kernel_tree(tree, tree.display().to_string()),
    }
}

fn inspect_kernel_tree(root: &Path, label: String) -> Result<PreparedKernel> {
    let kernel_image = find_unique_path(&root.join("boot"), "vmlinuz-")
        .with_context(|| format!("failed to locate kernel image under {}", root.display()))?;
    let kernel_release = kernel_image
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("vmlinuz-"))
        .map(str::to_owned);

    let modules_base = if root.join("lib/modules").is_dir() {
        Some(root.join("lib/modules"))
    } else if root.join("usr/lib/modules").is_dir() {
        Some(root.join("usr/lib/modules"))
    } else {
        None
    };

    let (modules_dir, release) = if let Some(modules_base) = modules_base {
        let release = if let Some(kernel_release) = kernel_release {
            if modules_base.join(&kernel_release).is_dir() {
                Some(kernel_release)
            } else {
                find_unique_subdir_name(&modules_base)?
            }
        } else {
            find_unique_subdir_name(&modules_base)?
        };
        let modules_dir = release
            .as_ref()
            .map(|release| modules_base.join(release))
            .filter(|path| path.is_dir());
        (modules_dir, release)
    } else {
        (None, kernel_release)
    };

    Ok(PreparedKernel {
        label,
        kernel_image,
        modules_dir,
        release,
    })
}

fn extract_kernel_archive(archive: &Path, destination: &Path) -> Result<()> {
    let file =
        fs::File::open(archive).with_context(|| format!("failed to open {}", archive.display()))?;
    let mut ar_archive = ArArchive::new(file);
    while let Some(entry) = ar_archive.next_entry() {
        let entry = entry.with_context(|| format!("failed to read {}", archive.display()))?;
        let identifier = String::from_utf8_lossy(entry.header().identifier())
            .trim()
            .trim_end_matches('/')
            .to_string();
        match identifier.as_str() {
            "data.tar.xz" => {
                unpack_tar(XzDecoder::new(entry), destination)?;
                return Ok(());
            }
            "data.tar.gz" => {
                unpack_tar(GzDecoder::new(entry), destination)?;
                return Ok(());
            }
            "data.tar.zst" => {
                unpack_tar(zstd::Decoder::new(entry)?, destination)?;
                return Ok(());
            }
            "data.tar" => {
                unpack_tar(entry, destination)?;
                return Ok(());
            }
            _ => {}
        }
    }

    bail!(
        "{} does not contain a supported data.tar payload",
        archive.display()
    )
}

fn unpack_tar<R: Read>(reader: R, destination: &Path) -> Result<()> {
    let mut archive = TarArchive::new(reader);
    archive
        .unpack(destination)
        .with_context(|| format!("failed to unpack into {}", destination.display()))
}

fn stage_guest_tree(
    guest_root: &Path,
    guest_init: &Path,
    test_binaries: &BTreeMap<String, PathBuf>,
    kernel: &PreparedKernel,
) -> Result<()> {
    fs::create_dir_all(guest_root)
        .with_context(|| format!("failed to create {}", guest_root.display()))?;

    copy_binary_with_runtime(guest_init, guest_root, Path::new("init"))?;
    copy_static_binary(&find_in_path("busybox")?, &guest_root.join("bin/busybox"))?;
    copy_binary_with_runtime(&find_in_path("ip")?, guest_root, Path::new("usr/sbin/ip"))?;

    for (name, binary) in test_binaries {
        copy_binary_with_runtime(binary, guest_root, &Path::new("tests").join(name))?;
    }

    if let Some(modules_dir) = &kernel.modules_dir {
        let release = kernel
            .release
            .as_ref()
            .context("kernel modules are present but the release could not be determined")?;
        let staged_modules = guest_root.join("lib/modules").join(release);
        copy_tree(modules_dir, &staged_modules)?;

        let status = Command::new("depmod")
            .arg("-b")
            .arg(guest_root)
            .arg(release)
            .status()
            .context("failed to run depmod")?;
        ensure!(status.success(), "depmod failed with {status}");
    }

    Ok(())
}

fn copy_static_binary(source: &Path, destination: &Path) -> Result<()> {
    copy_preserving_metadata(source, destination)
}

fn copy_binary_with_runtime(
    binary: &Path,
    guest_root: &Path,
    guest_relative_path: &Path,
) -> Result<()> {
    copy_preserving_metadata(binary, &guest_root.join(guest_relative_path))?;
    for library in ldd_dependencies(binary)? {
        let staged_library = guest_root.join(
            library
                .strip_prefix("/")
                .with_context(|| format!("unexpected relative library {}", library.display()))?,
        );
        copy_preserving_metadata(&library, &staged_library)?;
    }
    Ok(())
}

fn copy_preserving_metadata(source: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::copy(source, destination).with_context(|| {
        format!(
            "failed to copy {} to {}",
            source.display(),
            destination.display(),
        )
    })?;
    let permissions = fs::metadata(source)
        .with_context(|| format!("failed to stat {}", source.display()))?
        .permissions();
    fs::set_permissions(destination, permissions)
        .with_context(|| format!("failed to chmod {}", destination.display()))?;
    Ok(())
}

fn ldd_dependencies(binary: &Path) -> Result<BTreeSet<PathBuf>> {
    let output = Command::new("ldd")
        .arg(binary)
        .output()
        .with_context(|| format!("failed to run ldd on {}", binary.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}\n{stderr}");
    if combined.contains("not a dynamic executable") || combined.contains("statically linked") {
        return Ok(BTreeSet::new());
    }
    ensure!(
        output.status.success(),
        "ldd failed for {}:\n{combined}",
        binary.display()
    );

    let mut dependencies = BTreeSet::new();
    for line in combined
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if line.starts_with("linux-vdso") {
            continue;
        }
        if let Some((_, rhs)) = line.split_once("=>") {
            if let Some(path) = rhs.split_whitespace().next() {
                if path.starts_with('/') {
                    dependencies.insert(PathBuf::from(path));
                }
            }
            continue;
        }

        if let Some(path) = line.split_whitespace().next() {
            if path.starts_with('/') {
                dependencies.insert(PathBuf::from(path));
            }
        }
    }
    Ok(dependencies)
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    for entry in WalkDir::new(source) {
        let entry = entry.with_context(|| format!("failed to walk {}", source.display()))?;
        let relative = entry
            .path()
            .strip_prefix(source)
            .with_context(|| format!("failed to relativize {}", entry.path().display()))?;
        let staged = destination.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&staged)
                .with_context(|| format!("failed to create {}", staged.display()))?;
        } else if entry.file_type().is_symlink() {
            let target = fs::read_link(entry.path())
                .with_context(|| format!("failed to read {}", entry.path().display()))?;
            if let Some(parent) = staged.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            std::os::unix::fs::symlink(&target, &staged).with_context(|| {
                format!(
                    "failed to create symlink {} -> {}",
                    staged.display(),
                    target.display(),
                )
            })?;
        } else {
            copy_preserving_metadata(entry.path(), &staged)?;
        }
    }
    Ok(())
}

fn build_initramfs(guest_root: &Path, initramfs: &Path) -> Result<()> {
    let mut cmd = Command::new("cpio");
    let initramfs_file = fs::File::create(initramfs)
        .with_context(|| format!("failed to create {}", initramfs.display()))?;
    cmd.current_dir(guest_root)
        .arg("-o")
        .arg("-H")
        .arg("newc")
        .arg("--quiet")
        .stdin(Stdio::piped())
        .stdout(Stdio::from(initramfs_file));

    let mut child = cmd.spawn().context("failed to spawn cpio")?;
    {
        let stdin = child.stdin.as_mut().context("failed to open cpio stdin")?;
        for entry in WalkDir::new(guest_root).sort_by_file_name() {
            let entry =
                entry.with_context(|| format!("failed to walk {}", guest_root.display()))?;
            if entry.path() == guest_root {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(guest_root)
                .with_context(|| format!("failed to relativize {}", entry.path().display()))?;
            writeln!(stdin, "{}", relative.display())
                .with_context(|| format!("failed to write {}", relative.display()))?;
        }
    }

    let status = child.wait().context("failed to wait for cpio")?;
    ensure!(status.success(), "cpio failed with {status}");
    Ok(())
}

fn run_qemu_vm(
    kernel: &PreparedKernel,
    initramfs: &Path,
    run_args: &[String],
    qemu: &Path,
    timeout: Duration,
) -> Result<()> {
    let kernel_args = join_kernel_args(run_args);
    let mut cmd = Command::new(qemu);
    cmd.arg("-kernel")
        .arg(&kernel.kernel_image)
        .arg("-initrd")
        .arg(initramfs)
        .arg("-append")
        .arg(&kernel_args)
        .arg("-cpu")
        .arg("max")
        .arg("-m")
        .arg("1024")
        .arg("-smp")
        .arg("2")
        .arg("-display")
        .arg("none")
        .arg("-serial")
        .arg("stdio")
        .arg("-monitor")
        .arg("none")
        .arg("-no-reboot")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to launch {}", qemu.display()))?;
    let stdout = child.stdout.take().context("missing qemu stdout")?;
    let stderr = child.stderr.take().context("missing qemu stderr")?;
    let stdout_thread = thread::spawn(move || read_qemu_output(stdout));
    let stderr_thread = thread::spawn(move || read_qemu_output(stderr));
    let start = Instant::now();

    loop {
        if let Some(status) = child.try_wait().context("failed to poll qemu")? {
            let stdout = stdout_thread
                .join()
                .map_err(|_| anyhow!("qemu stdout reader panicked"))??;
            let stderr = stderr_thread
                .join()
                .map_err(|_| anyhow!("qemu stderr reader panicked"))??;
            let combined = format!("{stdout}{stderr}");
            print!("{stdout}");
            eprint!("{stderr}");

            ensure!(status.success(), "qemu exited with {status}\n{combined}");
            ensure!(
                combined.contains("init: success") && !combined.contains("init: failure"),
                "guest did not report success\n{combined}",
            );
            return Ok(());
        }

        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            let stdout = stdout_thread
                .join()
                .map_err(|_| anyhow!("qemu stdout reader panicked"))??;
            let stderr = stderr_thread
                .join()
                .map_err(|_| anyhow!("qemu stderr reader panicked"))??;
            bail!(
                "qemu timed out after {}s\n{}{}",
                timeout.as_secs(),
                stdout,
                stderr
            );
        }

        thread::sleep(Duration::from_millis(200));
    }
}

fn join_kernel_args(run_args: &[String]) -> String {
    let mut args = vec![
        "console=ttyS0".to_string(),
        "rdinit=/init".to_string(),
        "panic=-1".to_string(),
    ];
    args.extend(run_args.iter().map(|arg| format!("init.arg={arg}")));
    args.join(" ")
}

fn read_qemu_output(reader: impl Read) -> Result<String> {
    let mut output = String::new();
    let mut reader = BufReader::new(reader);
    reader.read_to_string(&mut output)?;
    Ok(output)
}

fn default_cache_dir() -> PathBuf {
    repo_root_hint_canonical().join("target/xdp-test-vm")
}

fn default_qemu() -> PathBuf {
    PathBuf::from("qemu-system-x86_64")
}

fn repo_root() -> Result<PathBuf> {
    Ok(repo_root_hint_canonical())
}

fn repo_root_hint() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn repo_root_hint_canonical() -> PathBuf {
    repo_root_hint()
        .canonicalize()
        .unwrap_or_else(|_| repo_root_hint())
}

fn resolve_user_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(env::current_dir()
        .context("failed to get current directory")?
        .join(path))
}

fn resolve_command_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() || path.components().count() > 1 {
        return resolve_user_path(path);
    }
    Ok(path.to_path_buf())
}

fn command_with_runner(runner: &str, program: &Path) -> Result<Command> {
    if runner == "none" {
        return Ok(Command::new(program));
    }

    let parts = shlex::split(runner).ok_or_else(|| anyhow!("invalid runner: {runner}"))?;
    let Some((runner_program, runner_args)) = parts.split_first() else {
        bail!("runner cannot be empty");
    };
    let mut cmd = Command::new(runner_program);
    cmd.args(runner_args).arg(program);
    Ok(cmd)
}

fn cargo_bin() -> PathBuf {
    env::var_os("CARGO")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("cargo"))
}

fn forward_cargo_env(cmd: &mut Command) {
    for key in [
        "CARGO_HOME",
        "RUSTUP_HOME",
        "RUSTUP_TOOLCHAIN",
        "PATH",
        "HOME",
        "TERM",
    ] {
        if let Some(value) = env::var_os(key) {
            cmd.env(key, value);
        }
    }
    cmd.env("CARGO", cargo_bin());
    if let Ok(rustc) = find_in_path("rustc") {
        cmd.env("RUSTC", rustc);
    }
    if let Ok(rustdoc) = find_in_path("rustdoc") {
        cmd.env("RUSTDOC", rustdoc);
    }
}

fn find_in_path(binary: &str) -> Result<PathBuf> {
    let path = env::var_os("PATH").ok_or_else(|| anyhow!("PATH is not set"))?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!("failed to locate `{binary}` in PATH")
}

fn find_unique_path(directory: &Path, prefix: &str) -> Result<PathBuf> {
    let mut matches = fs::read_dir(directory)
        .with_context(|| format!("failed to read {}", directory.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(prefix))
        })
        .collect::<Vec<_>>();
    matches.sort();
    ensure!(
        matches.len() == 1,
        "expected exactly one `{prefix}*` under {}, found {}",
        directory.display(),
        matches.len()
    );
    Ok(matches.remove(0))
}

fn find_unique_subdir_name(directory: &Path) -> Result<Option<String>> {
    let mut matches = fs::read_dir(directory)
        .with_context(|| format!("failed to read {}", directory.display()))?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect::<Vec<_>>();
    matches.sort();
    if matches.is_empty() {
        return Ok(None);
    }
    ensure!(
        matches.len() == 1,
        "expected exactly one modules release under {}, found {}",
        directory.display(),
        matches.len()
    );
    Ok(matches.pop())
}

fn archive_stem(path: &Path) -> String {
    safe_cache_name(
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("kernel"),
    )
}

fn safe_cache_name(name: &str) -> String {
    name.replace(['/', '.', '+', ':'], "_")
}

fn kernel_source_label(source: &KernelSource) -> String {
    match source {
        KernelSource::Archive(path) | KernelSource::Tree(path) => path.display().to_string(),
        KernelSource::ArchiveBundle { label, .. } => label.clone(),
    }
}
