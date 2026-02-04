use {
    crate::{home_dir, utils::spawn, Config},
    bzip2::bufread::BzDecoder,
    log::{debug, error, info, warn},
    regex::Regex,
    serde::{Deserialize, Serialize},
    solana_file_download::{download_file, download_file_with_headers},
    std::{
        env,
        ffi::OsString,
        fs::{self, File},
        io::{BufRead, BufReader, ErrorKind},
        path::{Path, PathBuf},
        process::{exit, Command},
        sync::OnceLock,
    },
    tar::Archive,
};

pub(crate) const DEFAULT_PLATFORM_TOOLS_VERSION: &str = "v1.53";
pub(crate) const DEFAULT_RUST_VERSION: &str = "1.89.0";

// Common headers used for Github API.
const USER_AGENT_HEADER: (&str, &str) = ("User-Agent", "cargo-build-sbf");
const GITHUB_API_VERSION_HEADER: (&str, &str) = ("X-GitHub-Api-Version", "2022-11-28");

// Headers necessary for querying Github and expecting a JSON response.
const GITHUB_API_JSON_RESPONSE_HEADERS: [(&str, &str); 3] = [
    USER_AGENT_HEADER,
    GITHUB_API_VERSION_HEADER,
    ("Accept", "application/vnd.github+json"),
];

// Headers necessary for downloading a file from Github API.
const GITHUB_API_BYTES_RESPONSE_HEADERS: [(&str, &str); 3] = [
    USER_AGENT_HEADER,
    GITHUB_API_VERSION_HEADER,
    ("Accept", "application/octet-stream"),
];

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

pub(crate) fn validate_platform_tools_version(
    requested_version: &str,
    builtin_version: &str,
) -> String {
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
            "Can't get the latest version of platform-tools: {err}. Using built-in version \
             {builtin_version}."
        );
        builtin_version.to_string()
    });
    let normalized_latest = semver_version(&latest_version);
    let latest_semver = semver::Version::parse(&normalized_latest).unwrap();
    if requested_semver <= latest_semver {
        downloadable_version(requested_version)
    } else {
        warn!(
            "Version {requested_version} is not valid, latest version is {latest_version}. Using \
             the built-in version {builtin_version}"
        );
        builtin_version.to_string()
    }
}

pub(crate) fn make_platform_tools_path_for_version(version: &str) -> PathBuf {
    home_dir()
        .join(".cache")
        .join("solana")
        .join(version)
        .join("platform-tools")
}

pub(crate) fn get_base_rust_version(platform_tools_version: &str) -> String {
    let target_path = make_platform_tools_path_for_version(platform_tools_version);
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

fn retrieve_file_from_github_api(
    download_file_name: &str,
    platform_tools_version: &str,
    download_file_path: &Path,
) -> Result<(), String> {
    #[derive(Debug, Deserialize, Serialize)]
    struct Items {
        name: String,
        url: String,
    }

    #[derive(Debug, Deserialize, Serialize)]
    struct GithubResponse {
        assets: Vec<Items>,
    }

    let client = reqwest::blocking::Client::new();
    let query_url = format!("https://api.github.com/repos/anza-xyz/platform-tools/releases/tags/{platform_tools_version}");

    let mut query_headers = reqwest::header::HeaderMap::new();
    for item in GITHUB_API_JSON_RESPONSE_HEADERS {
        query_headers.insert(item.0, item.1.parse().unwrap());
    }
    let response = client
        .get(query_url)
        .headers(query_headers)
        .send()
        .map_err(|err| format!("Failed to retrieve Github releases: {err}"))?;

    let parsed_response: GithubResponse = response
        .json()
        .map_err(|err| format!("Failed to parse Github response: {err}"))?;

    let download_url = parsed_response
        .assets
        .iter()
        .find(|item| item.name == download_file_name)
        .map(|item| item.url.as_str())
        .ok_or(format!("File {download_file_name} not found for download").as_str())?;

    download_file_with_headers(
        download_url,
        download_file_path,
        true,
        &mut None,
        &GITHUB_API_BYTES_RESPONSE_HEADERS,
    )
}

fn retrieve_file_from_browser_url(
    download_file_name: &str,
    platform_tools_version: &str,
    download_file_path: &Path,
) -> Result<(), String> {
    let url = format!("https://github.com/anza-xyz/platform-tools/releases/download/{platform_tools_version}/{download_file_name}");
    download_file(url.as_str(), download_file_path, true, &mut None)
}

fn download_platform_tools(
    download_file_name: &str,
    platform_tools_version: &str,
    download_file_path: &Path,
    use_rest_api: bool,
) -> Result<(), String> {
    if use_rest_api {
        retrieve_file_from_github_api(
            download_file_name,
            platform_tools_version,
            download_file_path,
        )
    } else {
        retrieve_file_from_browser_url(
            download_file_name,
            platform_tools_version,
            download_file_path,
        )
        .map_err(|err| {
            format!(
                "{err}\n It looks like the download has failed. If this is a persistent issue, \
                 try `cargo-build-sbf --install-only` to download from an alternative source."
            )
        })
    }
}

// Check whether a package is installed and install it if missing.
pub(crate) fn install_if_missing(
    config: &Config,
    platform_tools_version: &str,
    target_path: &Path,
    use_rest_api: bool,
) -> Result<(), String> {
    if config.force_tools_install && target_path.is_dir() {
        debug!("Remove directory {target_path:?}");
        fs::remove_dir_all(target_path)
            .map_err(|err| format!("could not remove {target_path:?}: {err}"))?;
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
        debug!("Remove directory {target_path:?}");
        fs::remove_dir(target_path)
            .map_err(|err| format!("could not remove {target_path:?}: {err}"))?;
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
            debug!("Remove file {target_path:?}");
            fs::remove_file(target_path)
                .map_err(|err| format!("could not remove {target_path:?}: {err}"))?;
        }

        fs::create_dir_all(target_path)
            .map_err(|err| format!("could not create {target_path:?}: {err}"))?;
        let arch = if cfg!(target_arch = "aarch64") {
            "aarch64"
        } else {
            "x86_64"
        };
        let platform_tools_download_file_name = if cfg!(target_os = "windows") {
            format!("platform-tools-windows-{arch}.tar.bz2")
        } else if cfg!(target_os = "macos") {
            format!("platform-tools-osx-{arch}.tar.bz2")
        } else {
            format!("platform-tools-linux-{arch}.tar.bz2")
        };

        let download_file_path = target_path.join(&platform_tools_download_file_name);
        if download_file_path.exists() {
            fs::remove_file(&download_file_path)
                .map_err(|err| format!("could not remove {download_file_path:?}: {err}"))?;
        }

        download_platform_tools(
            &platform_tools_download_file_name,
            platform_tools_version,
            &download_file_path,
            use_rest_api,
        )?;
        let zip = File::open(&download_file_path).map_err(|err| err.to_string())?;
        let tar = BzDecoder::new(BufReader::new(zip));
        let mut archive = Archive::new(tar);
        archive
            .unpack(target_path)
            .map_err(|err| format!("could not unpack downloaded archive: {err}"))?;
        fs::remove_file(download_file_path)
            .map_err(|err| format!("could not remove downloaded archive: {err}"))?;
        if should_nix_patch_bins_and_dylibs(config) {
            if let Err(e) = nix_patch_all_bins_and_dylibs(target_path) {
                error!(
                    "patching for nix failed ({e};) will continue, but tools might not work \
                     out-of-box"
                )
            }
        }
    }
    Ok(())
}

// Check if we have all binaries in place to execute the build command.
// If the download failed or the binaries were somehow deleted, inform the user how to fix it.
pub(crate) fn corrupted_toolchain(platform_tools_dir: &Path) -> bool {
    let toolchain_path = platform_tools_dir.join("rust");
    let binaries = toolchain_path.join("bin");

    let rustc = binaries.join(if cfg!(windows) { "rustc.exe" } else { "rustc" });
    let cargo = binaries.join(if cfg!(windows) { "cargo.exe" } else { "cargo" });

    !toolchain_path.try_exists().unwrap_or(false)
        || !binaries.try_exists().unwrap_or(false)
        || !rustc.try_exists().unwrap_or(false)
        || !cargo.try_exists().unwrap_or(false)
}

pub(crate) fn generate_toolchain_name(requested_toolchain_version: &str) -> String {
    if requested_toolchain_version == DEFAULT_PLATFORM_TOOLS_VERSION {
        return format!("{DEFAULT_RUST_VERSION}-sbpf-solana-{DEFAULT_PLATFORM_TOOLS_VERSION}");
    }

    let rustc_version_string = get_base_rust_version(requested_toolchain_version);
    // The version string has the format 'rustc 1.84.1'
    let mut it = rustc_version_string.split_whitespace();
    // Jump 'rustc'
    let _ = it.next();
    format!(
        "{}-sbpf-solana-{}",
        it.next().unwrap(),
        requested_toolchain_version
    )
}

// check whether custom solana toolchain is linked, and link it if it is not.
fn link_solana_toolchain(
    config: &Config,
    platform_tools_dir: &Path,
    requested_toolchain_version: &str,
) {
    let toolchain_path = platform_tools_dir.join("rust");
    let rustup = PathBuf::from("rustup");
    let rustup_args = vec!["toolchain", "list", "-v"];
    let rustup_output = spawn(
        &rustup,
        rustup_args,
        config.generate_child_script_on_failure,
    );
    if config.verbose {
        debug!("{rustup_output}");
    }
    let requested_toolchain_name = generate_toolchain_name(requested_toolchain_version);
    let mut do_link = true;
    for line in rustup_output.lines() {
        let substrings: Vec<&str> = line.split(' ').collect();
        let installed_toolchain_name = *substrings.first().unwrap();
        if installed_toolchain_name.contains("solana") {
            // Paths are always the last item in the output of 'rust toolchain list -v'
            let path = substrings.last();
            if *path.unwrap() != toolchain_path.to_str().unwrap()
                || requested_toolchain_name != installed_toolchain_name
            {
                // The toolchain name is always the first item in the output
                let rustup_args = vec!["toolchain", "uninstall", installed_toolchain_name];
                let output = spawn(
                    &rustup,
                    rustup_args,
                    config.generate_child_script_on_failure,
                );
                if config.verbose {
                    debug!("{output}");
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
            requested_toolchain_name.as_str(),
            toolchain_path.to_str().unwrap(),
        ];
        let output = spawn(
            &rustup,
            rustup_args,
            config.generate_child_script_on_failure,
        );
        if config.verbose {
            debug!("{output}");
        }
    }
}

pub(crate) fn install_tools(config: &Config, platform_tools_version: &str, use_rest_api: bool) {
    let target_path = make_platform_tools_path_for_version(platform_tools_version);
    install_if_missing(config, platform_tools_version, &target_path, use_rest_api).unwrap_or_else(
        |err| {
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
            error!("Failed to install platform-tools: {err}");
            exit(1);
        },
    );
}

pub(crate) fn install_and_link_tools(
    config: &Config,
    package: Option<&cargo_metadata::Package>,
    metadata: &cargo_metadata::Metadata,
) -> String {
    let platform_tools_version = config.platform_tools_version.unwrap_or_else(|| {
        let workspace_tools_version = metadata
            .workspace_metadata
            .get("solana")
            .and_then(|v| v.get("tools-version"))
            .and_then(|v| v.as_str());
        let package_tools_version = package
            .map(|p| {
                p.metadata
                    .get("solana")
                    .and_then(|v| v.get("tools-version"))
                    .and_then(|v| v.as_str())
            })
            .unwrap_or(None);
        match (workspace_tools_version, package_tools_version) {
            (Some(workspace_version), Some(package_version)) => {
                if workspace_version != package_version {
                    warn!(
                        "Workspace and package specify conflicting tools versions, \
                         {workspace_version} and {package_version}, using package version \
                         {package_version}"
                    );
                }
                package_version
            }
            (Some(workspace_version), None) => workspace_version,
            (None, Some(package_version)) => package_version,
            (None, None) => DEFAULT_PLATFORM_TOOLS_VERSION,
        }
    });

    let platform_tools_version =
        validate_platform_tools_version(platform_tools_version, DEFAULT_PLATFORM_TOOLS_VERSION);
    if !config.skip_tools_install {
        install_tools(config, &platform_tools_version, false);
    }

    if config.no_rustup_override {
        let target_triple = rust_target_triple(config);
        check_solana_target_installed(&target_triple);
    } else {
        let platform_tools_dir = make_platform_tools_path_for_version(&platform_tools_version);
        link_solana_toolchain(config, &platform_tools_dir, &platform_tools_version);
        // RUSTC variable overrides cargo +<toolchain> mechanism of
        // selecting the rust compiler and makes cargo run a rust compiler
        // other than the one linked in Solana toolchain. We have to prevent
        // this by removing RUSTC from the child process environment.
        if env::var("RUSTC").is_ok() {
            warn!(
                "Removed RUSTC from cargo environment, because it overrides +solana cargo command \
                 line option."
            );
            // Safety: cargo-build-sbf doesn't spawn any threads until final child process is spawned
            unsafe { env::remove_var("RUSTC") }
        }
    }

    platform_tools_version
}

// allow user to set proper `rustc` into RUSTC or into PATH
fn check_solana_target_installed(target: &str) {
    let rustc = env::var("RUSTC").unwrap_or("rustc".to_owned());
    let rustc = PathBuf::from(rustc);
    let output = spawn(&rustc, ["--print", "target-list"], false);
    if !output.contains(target) {
        error!(
            "Provided {rustc:?} does not have {target} target. The Solana rustc must be available \
             in $PATH or the $RUSTC environment variable for the build to succeed."
        );
        exit(1);
    }
}

pub(crate) fn rust_target_triple(config: &Config) -> String {
    let tools_version = semver::Version::parse(&semver_version(
        config
            .platform_tools_version
            .unwrap_or(DEFAULT_PLATFORM_TOOLS_VERSION),
    ))
    .unwrap();
    let sbpf_minimum_version = semver::Version::parse(&semver_version("v1.44")).unwrap();

    if config.arch == "v0" && tools_version < sbpf_minimum_version {
        "sbf-solana-solana".to_string()
    } else if config.arch == "v0" {
        "sbpf-solana-solana".to_string()
    } else {
        format!("sbpf{}-solana-solana", config.arch)
    }
}

fn nix_patch_all_bins_and_dylibs(path: &Path) -> Result<(), std::io::Error> {
    for libdir in [path.join("llvm/lib"), path.join("rust/lib")] {
        for candidate in std::fs::read_dir(libdir)? {
            let Ok(candidate) = candidate else { continue };
            if path_is_dylib(&candidate.path()) {
                nix_patch_bin_or_dylib(path, &candidate.path());
            }
        }
    }
    for bindir in [path.join("llvm/bin"), path.join("rust/bin")] {
        for candidate in std::fs::read_dir(bindir)? {
            let Ok(candidate) = candidate else { continue };
            nix_patch_bin_or_dylib(path, &candidate.path());
        }
    }
    for targetdir in std::fs::read_dir(path.join("rust/lib/rustlib"))? {
        let targetdir = targetdir?;
        for bindir in ["bin", "bin/gcc-ld"] {
            let Ok(bindir_candidates) = std::fs::read_dir(targetdir.path().join(bindir)) else {
                continue;
            };
            for candidate in bindir_candidates {
                let Ok(candidate) = candidate else { continue };
                nix_patch_bin_or_dylib(path, &candidate.path());
            }
        }
    }
    Ok(())
}

fn nix_patch_bin_or_dylib(out: &Path, fname: &Path) {
    debug!("attempting to patch {}", fname.display());
    // Only build `.nix-deps` once.
    static NIX_DEPS_DIR: OnceLock<PathBuf> = OnceLock::new();
    let mut nix_build_succeeded = true;
    let nix_deps_dir = NIX_DEPS_DIR.get_or_init(|| {
        // Run `nix-build` to "build" each dependency (which will likely reuse the existing
        // `/nix/store` copy, or at most download a pre-built copy).
        //
        // Importantly, we create a gc-root called `.nix-deps` in the target directory, but still
        // reference the actual `/nix/store` path in the rpath as it makes it significantly more
        // robust against changes to the location of the `.nix-deps` location.
        //
        // bintools: Needed for the path of `ld-linux.so` (via `nix-support/dynamic-linker`).
        // zlib: Needed as a system dependency of various LLVM tools.
        // patchelf: Needed for patching ELF binaries.
        // libgcc.lib: libstdc++
        let nix_deps_dir = out.join(".nix-deps");
        const NIX_EXPR: &str = "
        with (import <nixpkgs> {});
        symlinkJoin {
            name = \"solana-sbf-dependencies\";
            paths = [
                libedit
                python310
                ncurses
                zlib
                xz.out
                libxml2.out
                patchelf
                stdenv.cc.bintools
                libgcc.lib
            ];
        }
        ";
        nix_build_succeeded = Command::new("nix-build")
            .args([
                Path::new("-E"),
                Path::new(NIX_EXPR),
                Path::new("-o"),
                &nix_deps_dir,
            ])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        nix_deps_dir
    });
    if !nix_build_succeeded {
        return;
    }

    let mut patchelf = Command::new(nix_deps_dir.join("bin/patchelf"));
    patchelf.args(&[
        OsString::from("--add-rpath"),
        OsString::from(fs::canonicalize(nix_deps_dir).unwrap().join("lib")),
    ]);
    if !path_is_dylib(fname) {
        // Finally, set the correct .interp for binaries
        let dynamic_linker_path = nix_deps_dir.join("nix-support/dynamic-linker");
        let dynamic_linker = fs::read_to_string(dynamic_linker_path).unwrap();
        patchelf.args(["--set-interpreter", dynamic_linker.trim_end()]);
    }
    // Adjustments for lldb (which references debian/ubuntu specific sonames.)
    patchelf.args(["--replace-needed", "libedit.so.2", "libedit.so"]);
    patchelf.args(["--replace-needed", "libxml2.so.2", "libxml2.so"]);
    patchelf.arg(fname);
    let _ = patchelf.output();
}

fn path_is_dylib(path: &Path) -> bool {
    // The .so is not necessarily the extension, it might be libLLVM.so.18.1
    path.to_str().is_some_and(|path| path.contains(".so"))
}

fn should_nix_patch_bins_and_dylibs(config: &Config) -> bool {
    static SHOULD_FIX_BINS_AND_DYLIBS: OnceLock<bool> = OnceLock::new();
    let val = *SHOULD_FIX_BINS_AND_DYLIBS.get_or_init(|| {
        let uname = Command::new("uname").arg("-s").output();
        let Ok(output) = uname else {
            return false;
        };
        let output = output.stdout;
        if !output.starts_with(b"Linux") {
            return false;
        }
        // If the user has asked binaries to be patched for Nix, then
        // don't check for NixOS or `/lib`.
        // NOTE: this intentionally comes after the Linux check:
        // - patchelf only works with ELF files, so no need to run it on Mac or Windows
        // - On other Unix systems, there is no stable syscall interface, so Nix doesn't manage the
        // global libc.
        if let Some(explicit_value) = config.patch_binaries_for_nix {
            return explicit_value;
        }

        // Use `/etc/os-release` instead of `/etc/NIXOS`.
        // The latter one does not exist on NixOS when using tmpfs as root.
        let is_nixos = match File::open("/etc/os-release") {
            Err(e) if e.kind() == ErrorKind::NotFound => false,
            Err(e) => panic!("failed to access /etc/os-release: {e}"),
            Ok(os_release) => BufReader::new(os_release).lines().any(|l| {
                let l = l.expect("reading /etc/os-release");
                matches!(l.trim(), "ID=nixos" | "ID='nixos'" | "ID=\"nixos\"")
            }),
        };
        if !is_nixos {
            let in_nix_shell = env::var("IN_NIX_SHELL");
            if let Ok(in_nix_shell) = in_nix_shell {
                warn!(
                    "The IN_NIX_SHELL environment variable is `{in_nix_shell}`; you may need to \
                     set the --patch-binaries-for-nix argument"
                );
            }
        }
        is_nixos
    });
    if val {
        info!("You seem to be using Nix.");
    }
    val
}
