use {
    bzip2::bufread::BzDecoder,
    cargo_metadata::camino::Utf8PathBuf,
    clap::{crate_description, crate_name, crate_version, Arg},
    itertools::Itertools,
    log::*,
    regex::Regex,
    solana_file_download::download_file,
    solana_keypair::{write_keypair_file, Keypair},
    std::{
        borrow::Cow,
        collections::{HashMap, HashSet},
        env,
        ffi::OsStr,
        fs::{self, File},
        io::{prelude::*, BufReader, BufWriter},
        path::{Path, PathBuf},
        process::{exit, Command, Stdio},
        str::FromStr,
    },
    tar::Archive,
};

const DEFAULT_PLATFORM_TOOLS_VERSION: &str = "v1.46";

#[derive(Debug)]
struct Config<'a> {
    cargo_args: Vec<&'a str>,
    target_directory: Option<Utf8PathBuf>,
    sbf_out_dir: Option<PathBuf>,
    sbf_sdk: PathBuf,
    platform_tools_version: Option<&'a str>,
    dump: bool,
    features: Vec<String>,
    force_tools_install: bool,
    skip_tools_install: bool,
    no_rustup_override: bool,
    generate_child_script_on_failure: bool,
    no_default_features: bool,
    offline: bool,
    remap_cwd: bool,
    debug: bool,
    verbose: bool,
    workspace: bool,
    jobs: Option<String>,
    arch: &'a str,
    optimize_size: bool,
}

impl Default for Config<'_> {
    fn default() -> Self {
        Self {
            cargo_args: vec![],
            target_directory: None,
            sbf_sdk: env::current_exe()
                .expect("Unable to get current executable")
                .parent()
                .expect("Unable to get parent directory")
                .to_path_buf()
                .join("platform-tools-sdk")
                .join("sbf"),
            sbf_out_dir: None,
            platform_tools_version: None,
            dump: false,
            features: vec![],
            force_tools_install: false,
            skip_tools_install: false,
            no_rustup_override: false,
            generate_child_script_on_failure: false,
            no_default_features: false,
            offline: false,
            remap_cwd: true,
            debug: false,
            verbose: false,
            workspace: false,
            jobs: None,
            arch: "v0",
            optimize_size: false,
        }
    }
}

fn spawn<I, S>(program: &Path, args: I, generate_child_script_on_failure: bool) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args = Vec::from_iter(args);
    let msg = args
        .iter()
        .map(|arg| arg.as_ref().to_str().unwrap_or("?"))
        .join(" ");
    info!("spawn: {:?} {}", program, msg);

    let child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| {
            error!("Failed to execute {}: {}", program.display(), err);
            exit(1);
        });

    let output = child.wait_with_output().expect("failed to wait on child");
    if !output.status.success() {
        if !generate_child_script_on_failure {
            exit(1);
        }
        error!("cargo-build-sbf exited on command execution failure");
        let script_name = format!(
            "cargo-build-sbf-child-script-{}.sh",
            program.file_name().unwrap().to_str().unwrap(),
        );
        let file = File::create(&script_name).unwrap();
        let mut out = BufWriter::new(file);
        for (key, value) in env::vars() {
            writeln!(out, "{key}=\"{value}\" \\").unwrap();
        }
        write!(out, "{}", program.display()).unwrap();
        writeln!(out, "{}", msg).unwrap();
        out.flush().unwrap();
        error!(
            "To rerun the failed command for debugging use {}",
            script_name,
        );
        exit(1);
    }
    output
        .stdout
        .as_slice()
        .iter()
        .map(|&c| c as char)
        .collect::<String>()
}

pub fn is_version_string(arg: &str) -> Result<(), String> {
    let semver_re = Regex::new(r"^v?[0-9]+\.[0-9]+(\.[0-9]+)?").unwrap();
    if semver_re.is_match(arg) {
        return Ok(());
    }
    Err("a version string may start with 'v' and contains major and minor version numbers separated by a dot, e.g. v1.32 or 1.32".to_string())
}

fn home_dir() -> PathBuf {
    PathBuf::from(
        #[cfg_attr(not(windows), allow(clippy::unnecessary_lazy_evaluations))]
        env::var_os("HOME")
            .or_else(|| {
                #[cfg(windows)]
                {
                    debug!("Could not read env variable 'HOME', falling back to 'USERPROFILE'");
                    env::var_os("USERPROFILE")
                }

                #[cfg(not(windows))]
                {
                    None
                }
            })
            .unwrap_or_else(|| {
                error!("Can't get home directory path");
                exit(1);
            }),
    )
}

fn find_installed_platform_tools() -> Vec<String> {
    let solana = home_dir().join(".cache").join("solana");
    let package = "platform-tools";

    if let Ok(dir) = std::fs::read_dir(solana) {
        dir.filter_map(|e| match e {
            Err(_) => None,
            Ok(e) => {
                if e.path().join(package).is_dir() {
                    Some(e.path().file_name().unwrap().to_string_lossy().to_string())
                } else {
                    None
                }
            }
        })
        .collect::<Vec<_>>()
    } else {
        Vec::new()
    }
}

fn get_latest_platform_tools_version() -> Result<String, String> {
    let url = "https://github.com/anza-xyz/platform-tools/releases/latest";
    let resp = reqwest::blocking::get(url).map_err(|err| format!("Failed to GET {url}: {err}"))?;
    let path = std::path::Path::new(resp.url().path());
    let version = path.file_name().unwrap().to_string_lossy().to_string();
    Ok(version)
}

fn get_base_rust_version(platform_tools_version: &str) -> String {
    let target_path =
        make_platform_tools_path_for_version("platform-tools", platform_tools_version);
    let rustc = target_path.join("rust").join("bin").join("rustc");
    if !rustc.exists() {
        return String::from("");
    }
    let args = vec!["--version"];
    let output = spawn(&rustc, args, false);
    let rustc_re = Regex::new(r"(rustc [0-9]+\.[0-9]+\.[0-9]+).*").unwrap();
    if rustc_re.is_match(output.as_str()) {
        let captures = rustc_re.captures(output.as_str()).unwrap();
        captures[1].to_string()
    } else {
        String::from("")
    }
}

fn downloadable_version(version: &str) -> String {
    if version.starts_with('v') {
        version.to_string()
    } else {
        format!("v{version}")
    }
}

fn semver_version(version: &str) -> String {
    let starts_with_v = version.starts_with('v');
    let dots = version.as_bytes().iter().fold(
        0,
        |n: u32, c| if *c == b'.' { n.saturating_add(1) } else { n },
    );
    match (dots, starts_with_v) {
        (0, false) => format!("{version}.0.0"),
        (0, true) => format!("{}.0.0", &version[1..]),
        (1, false) => format!("{version}.0"),
        (1, true) => format!("{}.0", &version[1..]),
        (_, false) => version.to_string(),
        (_, true) => version[1..].to_string(),
    }
}

fn validate_platform_tools_version(requested_version: &str, builtin_version: &str) -> String {
    // Early return here in case it's the first time we're running `cargo build-sbf`
    // and we need to create the cache folders
    if requested_version == builtin_version {
        return builtin_version.to_string();
    }
    let normalized_requested = semver_version(requested_version);
    let requested_semver = semver::Version::parse(&normalized_requested).unwrap();
    let installed_versions = find_installed_platform_tools();
    for v in installed_versions {
        if requested_semver <= semver::Version::parse(&semver_version(&v)).unwrap() {
            return downloadable_version(requested_version);
        }
    }
    let latest_version = get_latest_platform_tools_version().unwrap_or_else(|err| {
        debug!(
            "Can't get the latest version of platform-tools: {}. Using built-in version {}.",
            err, builtin_version,
        );
        builtin_version.to_string()
    });
    let normalized_latest = semver_version(&latest_version);
    let latest_semver = semver::Version::parse(&normalized_latest).unwrap();
    if requested_semver <= latest_semver {
        downloadable_version(requested_version)
    } else {
        warn!(
            "Version {} is not valid, latest version is {}. Using the built-in version {}",
            requested_version, latest_version, builtin_version,
        );
        builtin_version.to_string()
    }
}

fn make_platform_tools_path_for_version(package: &str, version: &str) -> PathBuf {
    home_dir()
        .join(".cache")
        .join("solana")
        .join(version)
        .join(package)
}

// Check whether a package is installed and install it if missing.
fn install_if_missing(
    config: &Config,
    package: &str,
    url: &str,
    download_file_name: &str,
    platform_tools_version: &str,
    target_path: &Path,
) -> Result<(), String> {
    if config.force_tools_install {
        if target_path.is_dir() {
            debug!("Remove directory {:?}", target_path);
            fs::remove_dir_all(target_path).map_err(|err| err.to_string())?;
        }
        let source_base = config.sbf_sdk.join("dependencies");
        if source_base.exists() {
            let source_path = source_base.join(package);
            if source_path.exists() {
                debug!("Remove file {:?}", source_path);
                fs::remove_file(source_path).map_err(|err| err.to_string())?;
            }
        }
    }
    // Check whether the target path is an empty directory. This can
    // happen if package download failed on previous run of
    // cargo-build-sbf.  Remove the target_path directory in this
    // case.
    if target_path.is_dir()
        && target_path
            .read_dir()
            .map_err(|err| err.to_string())?
            .next()
            .is_none()
    {
        debug!("Remove directory {:?}", target_path);
        fs::remove_dir(target_path).map_err(|err| err.to_string())?;
    }

    // Check whether the package is already in ~/.cache/solana.
    // Download it and place in the proper location if not found.
    if !target_path.is_dir()
        && !target_path
            .symlink_metadata()
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
    {
        if target_path.exists() {
            debug!("Remove file {:?}", target_path);
            fs::remove_file(target_path).map_err(|err| err.to_string())?;
        }
        fs::create_dir_all(target_path).map_err(|err| err.to_string())?;
        let mut url = String::from(url);
        url.push('/');
        url.push_str(platform_tools_version);
        url.push('/');
        url.push_str(download_file_name);
        let download_file_path = target_path.join(download_file_name);
        if download_file_path.exists() {
            fs::remove_file(&download_file_path).map_err(|err| err.to_string())?;
        }
        download_file(url.as_str(), &download_file_path, true, &mut None)?;
        let zip = File::open(&download_file_path).map_err(|err| err.to_string())?;
        let tar = BzDecoder::new(BufReader::new(zip));
        let mut archive = Archive::new(tar);
        archive.unpack(target_path).map_err(|err| err.to_string())?;
        fs::remove_file(download_file_path).map_err(|err| err.to_string())?;
    }
    // Make a symbolic link source_path -> target_path in the
    // platform-tools-sdk/sbf/dependencies directory if no valid link found.
    let source_base = config.sbf_sdk.join("dependencies");
    if !source_base.exists() {
        fs::create_dir_all(&source_base).map_err(|err| err.to_string())?;
    }
    let source_path = source_base.join(package);
    // Check whether the correct symbolic link exists.
    let invalid_link = if let Ok(link_target) = source_path.read_link() {
        if link_target.ne(target_path) {
            fs::remove_file(&source_path).map_err(|err| err.to_string())?;
            true
        } else {
            false
        }
    } else {
        true
    };
    if invalid_link {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target_path, source_path).map_err(|err| err.to_string())?;
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(target_path, source_path)
            .map_err(|err| err.to_string())?;
    }
    Ok(())
}

// Process dump file attributing call instructions with callee function names
fn postprocess_dump(program_dump: &Path) {
    if !program_dump.exists() {
        return;
    }
    let postprocessed_dump = program_dump.with_extension("postprocessed");
    let head_re = Regex::new(r"^([0-9a-f]{16}) (.+)").unwrap();
    let insn_re = Regex::new(r"^ +([0-9]+)((\s[0-9a-f]{2})+)\s.+").unwrap();
    let call_re = Regex::new(r"^ +([0-9]+)(\s[0-9a-f]{2})+\scall (-?)0x([0-9a-f]+)").unwrap();
    let relo_re = Regex::new(r"^([0-9a-f]{16})  [0-9a-f]{16} R_BPF_64_32 +0{16} (.+)").unwrap();
    let mut a2n: HashMap<i64, String> = HashMap::new();
    let mut rel: HashMap<u64, String> = HashMap::new();
    let mut name = String::from("");
    let mut state = 0;
    let Ok(file) = File::open(program_dump) else {
        return;
    };
    for line_result in BufReader::new(file).lines() {
        let line = line_result.unwrap();
        let line = line.trim_end();
        if line == "Disassembly of section .text" {
            state = 1;
        }
        if state == 0 {
            if relo_re.is_match(line) {
                let captures = relo_re.captures(line).unwrap();
                let address = u64::from_str_radix(&captures[1], 16).unwrap();
                let symbol = captures[2].to_string();
                rel.insert(address, symbol);
            }
        } else if state == 1 {
            if head_re.is_match(line) {
                state = 2;
                let captures = head_re.captures(line).unwrap();
                name = captures[2].to_string();
            }
        } else if state == 2 {
            state = 1;
            if insn_re.is_match(line) {
                let captures = insn_re.captures(line).unwrap();
                let address = i64::from_str(&captures[1]).unwrap();
                a2n.insert(address, name.clone());
            }
        }
    }
    let Ok(file) = File::create(&postprocessed_dump) else {
        return;
    };
    let mut out = BufWriter::new(file);
    let Ok(file) = File::open(program_dump) else {
        return;
    };
    let mut pc = 0u64;
    let mut step = 0u64;
    for line_result in BufReader::new(file).lines() {
        let line = line_result.unwrap();
        let line = line.trim_end();
        if head_re.is_match(line) {
            let captures = head_re.captures(line).unwrap();
            pc = u64::from_str_radix(&captures[1], 16).unwrap();
            writeln!(out, "{line}").unwrap();
            continue;
        }
        if insn_re.is_match(line) {
            let captures = insn_re.captures(line).unwrap();
            step = if captures[2].len() > 24 { 16 } else { 8 };
        }
        if call_re.is_match(line) {
            if rel.contains_key(&pc) {
                writeln!(out, "{} ; {}", line, rel[&pc]).unwrap();
            } else {
                let captures = call_re.captures(line).unwrap();
                let pc = i64::from_str(&captures[1]).unwrap().checked_add(1).unwrap();
                let offset = i64::from_str_radix(&captures[4], 16).unwrap();
                let offset = if &captures[3] == "-" {
                    offset.checked_neg().unwrap()
                } else {
                    offset
                };
                let address = pc.checked_add(offset).unwrap();
                if a2n.contains_key(&address) {
                    writeln!(out, "{} ; {}", line, a2n[&address]).unwrap();
                } else {
                    writeln!(out, "{line}").unwrap();
                }
            }
        } else {
            writeln!(out, "{line}").unwrap();
        }
        pc = pc.checked_add(step).unwrap();
    }
    fs::rename(postprocessed_dump, program_dump).unwrap();
}

// Check whether the built .so file contains undefined symbols that are
// not known to the runtime and warn about them if any.
fn check_undefined_symbols(config: &Config, program: &Path) {
    let syscalls_txt = config.sbf_sdk.join("syscalls.txt");
    let Ok(file) = File::open(syscalls_txt) else {
        return;
    };
    let mut syscalls = HashSet::new();
    for line_result in BufReader::new(file).lines() {
        let line = line_result.unwrap();
        let line = line.trim_end();
        syscalls.insert(line.to_string());
    }
    let entry =
        Regex::new(r"^ *[0-9]+: [0-9a-f]{16} +[0-9a-f]+ +NOTYPE +GLOBAL +DEFAULT +UND +(.+)")
            .unwrap();
    let readelf = config
        .sbf_sdk
        .join("dependencies")
        .join("platform-tools")
        .join("llvm")
        .join("bin")
        .join("llvm-readelf");
    let mut readelf_args = vec!["--dyn-symbols"];
    readelf_args.push(program.to_str().unwrap());
    let output = spawn(
        &readelf,
        &readelf_args,
        config.generate_child_script_on_failure,
    );
    if config.verbose {
        debug!("{}", output);
    }
    let mut unresolved_symbols: Vec<String> = Vec::new();
    for line in output.lines() {
        let line = line.trim_end();
        if entry.is_match(line) {
            let captures = entry.captures(line).unwrap();
            let symbol = captures[1].to_string();
            if !syscalls.contains(&symbol) {
                unresolved_symbols.push(symbol);
            }
        }
    }
    if !unresolved_symbols.is_empty() {
        warn!(
            "The following functions are undefined and not known syscalls {:?}.",
            unresolved_symbols
        );
        warn!("         Calling them will trigger a run-time error.");
    }
}

// Check if we have all binaries in place to execute the build command.
// If the download failed or the binaries were somehow deleted, inform the user how to fix it.
fn corrupted_toolchain(config: &Config) -> bool {
    let toolchain_path = config
        .sbf_sdk
        .join("dependencies")
        .join("platform-tools")
        .join("rust");

    let binaries = toolchain_path.join("bin");

    !toolchain_path.try_exists().unwrap_or(false)
        || !binaries.try_exists().unwrap_or(false)
        || !binaries.join("rustc").try_exists().unwrap_or(false)
        || !binaries.join("cargo").try_exists().unwrap_or(false)
}

// check whether custom solana toolchain is linked, and link it if it is not.
fn link_solana_toolchain(config: &Config) {
    let toolchain_path = config
        .sbf_sdk
        .join("dependencies")
        .join("platform-tools")
        .join("rust");
    let rustup = PathBuf::from("rustup");
    let rustup_args = vec!["toolchain", "list", "-v"];
    let rustup_output = spawn(
        &rustup,
        rustup_args,
        config.generate_child_script_on_failure,
    );
    if config.verbose {
        debug!("{}", rustup_output);
    }
    let mut do_link = true;
    for line in rustup_output.lines() {
        if line.starts_with("solana") {
            let mut it = line.split_whitespace();
            let _ = it.next();
            let path = it.next();
            if path.unwrap() != toolchain_path.to_str().unwrap() {
                let rustup_args = vec!["toolchain", "uninstall", "solana"];
                let output = spawn(
                    &rustup,
                    rustup_args,
                    config.generate_child_script_on_failure,
                );
                if config.verbose {
                    debug!("{}", output);
                }
            } else {
                do_link = false;
            }
            break;
        }
    }
    if do_link {
        let rustup_args = vec![
            "toolchain",
            "link",
            "solana",
            toolchain_path.to_str().unwrap(),
        ];
        let output = spawn(
            &rustup,
            rustup_args,
            config.generate_child_script_on_failure,
        );
        if config.verbose {
            debug!("{}", output);
        }
    }
}

fn build_solana_package(
    config: &Config,
    target_directory: &Path,
    package: &cargo_metadata::Package,
    metadata: &cargo_metadata::Metadata,
) {
    let program_name = {
        let cdylib_targets = package
            .targets
            .iter()
            .filter_map(|target| {
                if target.crate_types.contains(&"cdylib".to_string()) {
                    let other_crate_type = if target.crate_types.contains(&"rlib".to_string()) {
                        Some("rlib")
                    } else if target.crate_types.contains(&"lib".to_string()) {
                        Some("lib")
                    } else {
                        None
                    };

                    if let Some(other_crate) = other_crate_type {
                        warn!("Package '{}' has two crate types defined: cdylib and {}. \
                        This setting precludes link-time optimizations (LTO). Use cdylib for programs \
                        to be deployed and rlib for packages to be imported by other programs as libraries.",
                        package.name, other_crate);
                    }

                    Some(&target.name)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        match cdylib_targets.len() {
            0 => None,
            1 => Some(cdylib_targets[0].replace('-', "_")),
            _ => {
                error!(
                    "{} crate contains multiple cdylib targets: {:?}",
                    package.name, cdylib_targets
                );
                exit(1);
            }
        }
    };

    let legacy_program_feature_present = package.name == "solana-sdk";
    let root_package_dir = &package.manifest_path.parent().unwrap_or_else(|| {
        error!("Unable to get directory of {}", package.manifest_path);
        exit(1);
    });

    let sbf_out_dir = config
        .sbf_out_dir
        .as_ref()
        .cloned()
        .unwrap_or_else(|| target_directory.join("deploy"));

    let target_triple = if config.arch == "v0" {
        "sbpf-solana-solana".to_string()
    } else {
        format!("sbpf{}-solana-solana", config.arch)
    };

    let target_build_directory = target_directory.join(&target_triple).join("release");

    env::set_current_dir(root_package_dir).unwrap_or_else(|err| {
        error!(
            "Unable to set current directory to {}: {}",
            root_package_dir, err
        );
        exit(1);
    });

    let platform_tools_version = config.platform_tools_version.unwrap_or_else(|| {
        let workspace_tools_version = metadata.workspace_metadata.get("solana").and_then(|v| v.get("tools-version")).and_then(|v| v.as_str());
        let package_tools_version = package.metadata.get("solana").and_then(|v| v.get("tools-version")).and_then(|v| v.as_str());
        match (workspace_tools_version, package_tools_version) {
            (Some(workspace_version), Some(package_version)) => {
                if workspace_version != package_version {
                    warn!("Workspace and package specify conflicting tools versions, {workspace_version} and {package_version}, using package version {package_version}");
                }
                package_version
            },
            (Some(workspace_version), None) => workspace_version,
            (None, Some(package_version)) => package_version,
            (None, None) => DEFAULT_PLATFORM_TOOLS_VERSION,
        }
    });

    info!("Solana SDK: {}", config.sbf_sdk.display());
    if config.no_default_features {
        info!("No default features");
    }
    if !config.features.is_empty() {
        info!("Features: {}", config.features.join(" "));
    }
    if legacy_program_feature_present {
        info!("Legacy program feature detected");
    }
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    };

    if !config.skip_tools_install {
        let platform_tools_version =
            validate_platform_tools_version(platform_tools_version, DEFAULT_PLATFORM_TOOLS_VERSION);

        let platform_tools_download_file_name = if cfg!(target_os = "windows") {
            format!("platform-tools-windows-{arch}.tar.bz2")
        } else if cfg!(target_os = "macos") {
            format!("platform-tools-osx-{arch}.tar.bz2")
        } else {
            format!("platform-tools-linux-{arch}.tar.bz2")
        };
        let package = "platform-tools";
        let target_path = make_platform_tools_path_for_version(package, &platform_tools_version);
        install_if_missing(
            config,
            package,
            "https://github.com/anza-xyz/platform-tools/releases/download",
            platform_tools_download_file_name.as_str(),
            &platform_tools_version,
            &target_path,
        )
        .unwrap_or_else(|err| {
            // The package version directory doesn't contain a valid
            // installation, and it should be removed.
            let target_path_parent = target_path.parent().expect("Invalid package path");
            if target_path_parent.exists() {
                fs::remove_dir_all(target_path_parent).unwrap_or_else(|err| {
                    error!(
                        "Failed to remove {} while recovering from installation failure: {}",
                        target_path_parent.to_string_lossy(),
                        err,
                    );
                    exit(1);
                });
            }
            error!("Failed to install platform-tools: {}", err);
            exit(1);
        });
    }

    if config.no_rustup_override {
        check_solana_target_installed(&target_triple);
    } else {
        link_solana_toolchain(config);
        // RUSTC variable overrides cargo +<toolchain> mechanism of
        // selecting the rust compiler and makes cargo run a rust compiler
        // other than the one linked in Solana toolchain. We have to prevent
        // this by removing RUSTC from the child process environment.
        if env::var("RUSTC").is_ok() {
            warn!(
                "Removed RUSTC from cargo environment, because it overrides +solana cargo command line option."
            );
            env::remove_var("RUSTC")
        }
    }

    if corrupted_toolchain(config) {
        error!(
            "The Solana toolchain is corrupted. Please, run cargo-build-sbf with the \
        --force-tools-install argument to fix it."
        );
        exit(1);
    }

    let llvm_bin = config
        .sbf_sdk
        .join("dependencies")
        .join("platform-tools")
        .join("llvm")
        .join("bin");
    env::set_var("CC", llvm_bin.join("clang"));
    env::set_var("AR", llvm_bin.join("llvm-ar"));
    env::set_var("OBJDUMP", llvm_bin.join("llvm-objdump"));
    env::set_var("OBJCOPY", llvm_bin.join("llvm-objcopy"));

    let cargo_target = format!(
        "CARGO_TARGET_{}_RUSTFLAGS",
        target_triple.to_uppercase().replace("-", "_")
    );
    let rustflags = env::var("RUSTFLAGS").ok().unwrap_or_default();
    if env::var("RUSTFLAGS").is_ok() {
        warn!(
            "Removed RUSTFLAGS from cargo environment, because it overrides {}.",
            cargo_target,
        );
        env::remove_var("RUSTFLAGS")
    }
    let target_rustflags = env::var(&cargo_target).ok();
    let mut target_rustflags = Cow::Borrowed(target_rustflags.as_deref().unwrap_or_default());
    target_rustflags = Cow::Owned(format!("{} {}", &rustflags, &target_rustflags));
    if config.remap_cwd && !config.debug {
        target_rustflags = Cow::Owned(format!("{} -Zremap-cwd-prefix=", &target_rustflags));
    }
    if config.optimize_size {
        target_rustflags = Cow::Owned(format!("{} -C opt-level=s", &target_rustflags));
    }
    if config.debug {
        // Replace with -Zsplit-debuginfo=packed when stabilized.
        target_rustflags = Cow::Owned(format!("{} -g", &target_rustflags));
    }
    if let Cow::Owned(flags) = target_rustflags {
        env::set_var(&cargo_target, flags);
    }
    if config.verbose {
        debug!(
            "{}=\"{}\"",
            cargo_target,
            env::var(&cargo_target).ok().unwrap_or_default(),
        );
    }

    let cargo_build = PathBuf::from("cargo");
    let mut cargo_build_args = vec![];
    if !config.no_rustup_override {
        cargo_build_args.push("+solana");
    };

    cargo_build_args.append(&mut vec!["build", "--release", "--target", &target_triple]);
    if config.no_default_features {
        cargo_build_args.push("--no-default-features");
    }
    for feature in &config.features {
        cargo_build_args.push("--features");
        cargo_build_args.push(feature);
    }
    if legacy_program_feature_present {
        if !config.no_default_features {
            cargo_build_args.push("--no-default-features");
        }
        cargo_build_args.push("--features=program");
    }
    if config.verbose {
        cargo_build_args.push("--verbose");
    }
    if let Some(jobs) = &config.jobs {
        cargo_build_args.push("--jobs");
        cargo_build_args.push(jobs);
    }
    cargo_build_args.append(&mut config.cargo_args.clone());
    let output = spawn(
        &cargo_build,
        &cargo_build_args,
        config.generate_child_script_on_failure,
    );
    if config.verbose {
        debug!("{}", output);
    }

    if let Some(program_name) = program_name {
        let program_unstripped_so = target_build_directory.join(format!("{program_name}.so"));
        let program_dump = sbf_out_dir.join(format!("{program_name}-dump.txt"));
        let program_so = sbf_out_dir.join(format!("{program_name}.so"));
        let program_debug = sbf_out_dir.join(format!("{program_name}.debug"));
        let program_keypair = sbf_out_dir.join(format!("{program_name}-keypair.json"));

        fn file_older_or_missing(prerequisite_file: &Path, target_file: &Path) -> bool {
            let prerequisite_metadata = fs::metadata(prerequisite_file).unwrap_or_else(|err| {
                error!(
                    "Unable to get file metadata for {}: {}",
                    prerequisite_file.display(),
                    err
                );
                exit(1);
            });

            if let Ok(target_metadata) = fs::metadata(target_file) {
                use std::time::UNIX_EPOCH;
                prerequisite_metadata.modified().unwrap_or(UNIX_EPOCH)
                    > target_metadata.modified().unwrap_or(UNIX_EPOCH)
            } else {
                true
            }
        }

        if !program_keypair.exists() {
            write_keypair_file(&Keypair::new(), &program_keypair).unwrap_or_else(|err| {
                error!(
                    "Unable to get create {}: {}",
                    program_keypair.display(),
                    err
                );
                exit(1);
            });
        }

        if file_older_or_missing(&program_unstripped_so, &program_so) {
            #[cfg(windows)]
            let output = spawn(
                &llvm_bin.join("llvm-objcopy"),
                [
                    "--strip-all".as_ref(),
                    program_unstripped_so.as_os_str(),
                    program_so.as_os_str(),
                ],
                config.generate_child_script_on_failure,
            );
            #[cfg(not(windows))]
            let output = spawn(
                &config.sbf_sdk.join("scripts").join("strip.sh"),
                [&program_unstripped_so, &program_so],
                config.generate_child_script_on_failure,
            );
            if config.verbose {
                debug!("{}", output);
            }
        }

        if config.dump && file_older_or_missing(&program_unstripped_so, &program_dump) {
            let dump_script = config.sbf_sdk.join("scripts").join("dump.sh");
            #[cfg(windows)]
            {
                error!("Using Bash scripts from within a program is not supported on Windows, skipping `--dump`.");
                error!(
                    "Please run \"{} {} {}\" from a Bash-supporting shell, then re-run this command to see the processed program dump.",
                    &dump_script.display(),
                    &program_unstripped_so.display(),
                    &program_dump.display());
            }
            #[cfg(not(windows))]
            {
                let output = spawn(
                    &dump_script,
                    [&program_unstripped_so, &program_dump],
                    config.generate_child_script_on_failure,
                );
                if config.verbose {
                    debug!("{}", output);
                }
            }
            postprocess_dump(&program_dump);
        }

        if config.debug && file_older_or_missing(&program_unstripped_so, &program_debug) {
            #[cfg(windows)]
            let llvm_objcopy = &llvm_bin.join("llvm-objcopy");
            #[cfg(not(windows))]
            let llvm_objcopy = &config.sbf_sdk.join("scripts").join("objcopy.sh");

            let output = spawn(
                llvm_objcopy,
                [
                    "--only-keep-debug".as_ref(),
                    program_unstripped_so.as_os_str(),
                    program_debug.as_os_str(),
                ],
                config.generate_child_script_on_failure,
            );
            if config.verbose {
                debug!("{}", output);
            }
        }

        if config.arch != "v3" {
            // SBPFv3 shall not have any undefined syscall.
            check_undefined_symbols(config, &program_so);
        }

        info!("To deploy this program:");
        info!("  $ solana program deploy {}", program_so.display());
        info!("The program address will default to this keypair (override with --program-id):");
        info!("  {}", program_keypair.display());
    } else if config.dump {
        warn!("Note: --dump is only available for crates with a cdylib target");
    }
}

// allow user to set proper `rustc` into RUSTC or into PATH
fn check_solana_target_installed(target: &str) {
    let rustc = env::var("RUSTC").unwrap_or("rustc".to_owned());
    let rustc = PathBuf::from(rustc);
    let output = spawn(&rustc, ["--print", "target-list"], false);
    if !output.contains(target) {
        error!("Provided {:?} does not have {} target. The Solana rustc must be available in $PATH or the $RUSTC environment variable for the build to succeed.", rustc, target);
        exit(1);
    }
}

fn build_solana(config: Config, manifest_path: Option<PathBuf>) {
    let mut metadata_command = cargo_metadata::MetadataCommand::new();
    if let Some(manifest_path) = manifest_path {
        metadata_command.manifest_path(manifest_path);
    }
    if config.offline {
        metadata_command.other_options(vec!["--offline".to_string()]);
    }

    let metadata = metadata_command.exec().unwrap_or_else(|err| {
        error!("Failed to obtain package metadata: {}", err);
        exit(1);
    });

    let target_dir = config
        .target_directory
        .clone()
        .unwrap_or(metadata.target_directory.clone());

    if let Some(root_package) = metadata.root_package() {
        if !config.workspace {
            build_solana_package(&config, target_dir.as_ref(), root_package, &metadata);
            return;
        }
    }

    let all_sbf_packages = metadata
        .packages
        .iter()
        .filter(|package| {
            if metadata.workspace_members.contains(&package.id) {
                for target in package.targets.iter() {
                    if target.kind.contains(&"cdylib".to_string()) {
                        return true;
                    }
                }
            }
            false
        })
        .collect::<Vec<_>>();

    for package in all_sbf_packages {
        build_solana_package(&config, target_dir.as_ref(), package, &metadata);
    }
}

fn main() {
    solana_logger::setup();
    let default_config = Config::default();
    let default_sbf_sdk = format!("{}", default_config.sbf_sdk.display());

    let mut args = env::args().collect::<Vec<_>>();
    // When run as a cargo subcommand, the first program argument is the subcommand name.
    // Remove it
    if let Some(arg1) = args.get(1) {
        if arg1 == "build-sbf" {
            args.remove(1);
        }
    }

    // The following line is scanned by CI configuration script to
    // separate cargo caches according to the version of platform-tools.
    let rust_base_version = get_base_rust_version(DEFAULT_PLATFORM_TOOLS_VERSION);
    let version = format!(
        "{}\nplatform-tools {}\n{}",
        crate_version!(),
        DEFAULT_PLATFORM_TOOLS_VERSION,
        rust_base_version,
    );
    let matches = clap::Command::new(crate_name!())
        .about(crate_description!())
        .version(version.as_str())
        .arg(
            Arg::new("sbf_out_dir")
                .env("SBF_OUT_PATH")
                .long("sbf-out-dir")
                .value_name("DIRECTORY")
                .takes_value(true)
                .help("Place final SBF build artifacts in this directory"),
        )
        .arg(
            Arg::new("sbf_sdk")
                .env("SBF_SDK_PATH")
                .long("sbf-sdk")
                .value_name("PATH")
                .takes_value(true)
                .default_value(&default_sbf_sdk)
                .help("Path to the Solana SBF SDK"),
        )
        .arg(
            Arg::new("cargo_args")
                .help("Arguments passed directly to `cargo build`")
                .multiple_occurrences(true)
                .multiple_values(true)
                .last(true),
        )
        .arg(
            Arg::new("remap_cwd")
                .long("disable-remap-cwd")
                .takes_value(false)
                .help("Disable remap of cwd prefix and preserve full path strings in binaries"),
        )
        .arg(
            Arg::new("debug")
                .long("debug")
                .takes_value(false)
                .help("Enable debug symbols"),
        )
        .arg(
            Arg::new("dump")
                .long("dump")
                .takes_value(false)
                .help("Dump ELF information to a text file on success"),
        )
        .arg(
            Arg::new("features")
                .long("features")
                .value_name("FEATURES")
                .takes_value(true)
                .multiple_occurrences(true)
                .multiple_values(true)
                .help("Space-separated list of features to activate"),
        )
        .arg(
            Arg::new("force_tools_install")
                .long("force-tools-install")
                .takes_value(false)
                .conflicts_with("skip_tools_install")
                .help("Download and install platform-tools even when existing tools are located"),
        )
        .arg(
            Arg::new("skip_tools_install")
                .long("skip-tools-install")
                .takes_value(false)
                .conflicts_with("force_tools_install")
                .help("Skip downloading and installing platform-tools, assuming they are properly mounted"),
            )
            .arg(
                Arg::new("no_rustup_override")
                .long("no-rustup-override")
                .takes_value(false)
                .conflicts_with("force_tools_install")
                .help("Do not use rustup to manage the toolchain. By default, cargo-build-sbf invokes rustup to find the Solana rustc using a `+solana` toolchain override. This flag disables that behavior."),
        )
        .arg(
            Arg::new("generate_child_script_on_failure")
                .long("generate-child-script-on-failure")
                .takes_value(false)
                .help("Generate a shell script to rerun a failed subcommand"),
        )
        .arg(
            Arg::new("manifest_path")
                .long("manifest-path")
                .value_name("PATH")
                .takes_value(true)
                .help("Path to Cargo.toml"),
        )
        .arg(
            Arg::new("no_default_features")
                .long("no-default-features")
                .takes_value(false)
                .help("Do not activate the `default` feature"),
        )
        .arg(
            Arg::new("offline")
                .long("offline")
                .takes_value(false)
                .help("Run without accessing the network"),
        )
        .arg(
            Arg::new("tools_version")
                .long("tools-version")
                .value_name("STRING")
                .takes_value(true)
                .validator(is_version_string)
                .help(
                    "platform-tools version to use or to install, a version string, e.g. \"v1.32\"",
                ),
        )
        .arg(
            Arg::new("verbose")
                .short('v')
                .long("verbose")
                .takes_value(false)
                .help("Use verbose output"),
        )
        .arg(
            Arg::new("workspace")
                .long("workspace")
                .takes_value(false)
                .alias("all")
                .help("Build all Solana packages in the workspace"),
        )
        .arg(
            Arg::new("jobs")
                .short('j')
                .long("jobs")
                .takes_value(true)
                .value_name("N")
                .validator(|val| val.parse::<usize>().map_err(|e| e.to_string()))
                .help("Number of parallel jobs, defaults to # of CPUs"),
        )
        .arg(
            Arg::new("arch")
                .long("arch")
                .possible_values(["v0", "v1", "v2", "v3"])
                .default_value("v0")
                .help("Build for the given target architecture"),
        )
        .arg(
            Arg::new("optimize_size")
                .long("optimize-size")
                .takes_value(false)
                .help("Optimize program for size. This option may reduce program size, potentially increasing CU consumption.")
        )
        .get_matches_from(args);

    let sbf_sdk: PathBuf = matches.value_of_t_or_exit("sbf_sdk");
    let sbf_out_dir: Option<PathBuf> = matches.value_of_t("sbf_out_dir").ok();

    let mut cargo_args = matches
        .values_of("cargo_args")
        .map(|vals| vals.collect::<Vec<_>>())
        .unwrap_or_default();

    let target_dir_string;
    let target_directory = if let Some(target_dir) = cargo_args
        .iter_mut()
        .skip_while(|x| x != &&"--target-dir")
        .nth(1)
    {
        let target_path = Utf8PathBuf::from(*target_dir);
        // Directory needs to exist in order to canonicalize it
        fs::create_dir_all(&target_path).unwrap_or_else(|err| {
            error!("Unable to create target-dir directory {target_dir}: {err}");
            exit(1);
        });
        // Canonicalize the path to avoid issues with relative paths
        let canonicalized = target_path.canonicalize_utf8().unwrap_or_else(|err| {
            error!("Unable to canonicalize provided target-dir directory {target_path}: {err}");
            exit(1);
        });
        target_dir_string = canonicalized.to_string();
        *target_dir = &target_dir_string;
        Some(canonicalized)
    } else {
        None
    };

    let config = Config {
        cargo_args,
        target_directory,
        sbf_sdk: fs::canonicalize(&sbf_sdk).unwrap_or_else(|err| {
            error!(
                "Solana SDK path does not exist: {}: {}",
                sbf_sdk.display(),
                err
            );
            exit(1);
        }),
        sbf_out_dir: sbf_out_dir.map(|sbf_out_dir| {
            if sbf_out_dir.is_absolute() {
                sbf_out_dir
            } else {
                env::current_dir()
                    .expect("Unable to get current working directory")
                    .join(sbf_out_dir)
            }
        }),
        platform_tools_version: matches.value_of("tools_version"),
        dump: matches.is_present("dump"),
        features: matches.values_of_t("features").ok().unwrap_or_default(),
        force_tools_install: matches.is_present("force_tools_install"),
        skip_tools_install: matches.is_present("skip_tools_install"),
        no_rustup_override: matches.is_present("no_rustup_override"),
        generate_child_script_on_failure: matches.is_present("generate_child_script_on_failure"),
        no_default_features: matches.is_present("no_default_features"),
        remap_cwd: !matches.is_present("remap_cwd"),
        debug: matches.is_present("debug"),
        offline: matches.is_present("offline"),
        verbose: matches.is_present("verbose"),
        workspace: matches.is_present("workspace"),
        jobs: matches.value_of_t("jobs").ok(),
        arch: matches.value_of("arch").unwrap(),
        optimize_size: matches.is_present("optimize_size"),
    };
    let manifest_path: Option<PathBuf> = matches.value_of_t("manifest_path").ok();
    if config.verbose {
        debug!("{:?}", config);
        debug!("manifest_path: {:?}", manifest_path);
    }
    build_solana(config, manifest_path);
}
