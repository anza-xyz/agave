mod post_processing;
mod toolchain;
mod utils;

use {
    crate::{
        post_processing::post_process,
        toolchain::{
            corrupted_toolchain, generate_toolchain_name, get_base_rust_version,
            install_and_link_tools, install_tools, rust_target_triple,
            validate_platform_tools_version, DEFAULT_PLATFORM_TOOLS_VERSION,
        },
        utils::spawn,
    },
    cargo_metadata::camino::Utf8PathBuf,
    clap::{crate_description, crate_name, crate_version, Arg},
    log::*,
    regex::Regex,
    std::{
        borrow::Cow,
        env,
        fs::{self},
        path::PathBuf,
        process::exit,
    },
};

#[derive(Debug)]
pub struct Config<'a> {
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
    quiet: bool,
    workspace: bool,
    jobs: Option<String>,
    arch: &'a str,
    optimize_size: bool,
    lto: bool,
    install_only: bool,
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
            quiet: false,
            workspace: false,
            jobs: None,
            arch: "v0",
            optimize_size: false,
            lto: false,
            install_only: false,
        }
    }
}

pub fn is_version_string(arg: &str) -> Result<(), String> {
    let semver_re = Regex::new(r"^v?[0-9]+\.[0-9]+(\.[0-9]+)?$").unwrap();
    if semver_re.is_match(arg) {
        return Ok(());
    }
    Err(
        "a version string may start with 'v' and contains major and minor version numbers \
         separated by a dot, e.g. v1.32 or 1.32"
            .to_string(),
    )
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

fn prepare_environment(
    config: &Config,
    package: Option<&cargo_metadata::Package>,
    metadata: &cargo_metadata::Metadata,
) -> String {
    let root_dir = if let Some(package) = package {
        &package.manifest_path.parent().unwrap_or_else(|| {
            error!("Unable to get directory of {}", package.manifest_path);
            exit(1);
        })
    } else {
        &&*metadata.workspace_root
    };

    env::set_current_dir(root_dir).unwrap_or_else(|err| {
        error!("Unable to set current directory to {root_dir}: {err}");
        exit(1);
    });

    install_and_link_tools(config, package, metadata)
}

fn invoke_cargo(config: &Config, validated_toolchain_version: String) {
    let target_triple = rust_target_triple(config);

    info!("Solana SDK: {}", config.sbf_sdk.display());
    if config.no_default_features {
        info!("No default features");
    }
    if !config.features.is_empty() {
        info!("Features: {}", config.features.join(" "));
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
        warn!("Removed RUSTFLAGS from cargo environment, because it overrides {cargo_target}.");
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
    if config.lto {
        target_rustflags = Cow::Owned(format!(
            "{} -C embed-bitcode=yes -C lto=fat",
            &target_rustflags
        ));
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

    let mut toolchain_name: String;
    if !config.no_rustup_override {
        toolchain_name = generate_toolchain_name(validated_toolchain_version.as_str());
        toolchain_name = format!("+{toolchain_name}");
        cargo_build_args.push(toolchain_name.as_str());
    };

    cargo_build_args.append(&mut vec!["build", "--target", &target_triple]);
    if !config.debug {
        cargo_build_args.push("--release");
    }
    if config.no_default_features {
        cargo_build_args.push("--no-default-features");
    }
    for feature in &config.features {
        cargo_build_args.push("--features");
        cargo_build_args.push(feature);
    }
    if config.verbose {
        cargo_build_args.push("--verbose");
    }
    if config.quiet {
        cargo_build_args.push("--quiet");
    }
    if let Some(jobs) = &config.jobs {
        cargo_build_args.push("--jobs");
        cargo_build_args.push(jobs);
    }
    if config.workspace {
        cargo_build_args.push("--workspace");
    }
    cargo_build_args.append(&mut config.cargo_args.clone());
    let output = spawn(
        &cargo_build,
        &cargo_build_args,
        config.generate_child_script_on_failure,
    );

    if config.verbose {
        debug!("{output}");
    }
}

fn generate_program_name(package: &cargo_metadata::Package) -> Option<String> {
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
                    warn!(
                        "Package '{}' has two crate types defined: cdylib and {}. This setting \
                         precludes link-time optimizations (LTO). Use cdylib for programs to be \
                         deployed and rlib for packages to be imported by other programs as \
                         libraries.",
                        package.name, other_crate
                    );
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
        error!("Failed to obtain package metadata: {err}");
        exit(1);
    });

    let target_dir = config
        .target_directory
        .clone()
        .unwrap_or(metadata.target_directory.clone());

    if let Some(root_package) = metadata.root_package() {
        if !config.workspace {
            let program_name = generate_program_name(root_package);
            let validated_toolchain_version =
                prepare_environment(&config, Some(root_package), &metadata);
            invoke_cargo(&config, validated_toolchain_version);
            post_process(&config, target_dir.as_ref(), program_name);
            return;
        }
    }

    let validated_toolchain_version = prepare_environment(&config, None, &metadata);
    invoke_cargo(&config, validated_toolchain_version);

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
        let program_name = generate_program_name(package);
        post_process(&config, target_dir.as_ref(), program_name);
    }
}

fn main() {
    agave_logger::setup();
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
        .arg(Arg::new("debug").long("debug").takes_value(false).help(
            "Create debug objects at \
             `target/deploy/debug`.\n`target/deploy/debug/program.so.debug` contains all debug \
             information available.\n`target/deploy/debug/program.so` is a stripped version for \
             execution in the VM.\nThese objects are not optimized for mainnet-beta deployment.",
        ))
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
            Arg::new("install_only")
                .long("install-only")
                .takes_value(false)
                .conflicts_with("skip_tools_install")
                .help(
                    "Download and install platform-tools, without building a program. May be used \
                     together with `--force-tools-install` to override an existing installation. \
                     To choose a specific version, use `--install-only --tools-version v1.51`",
                ),
        )
        .arg(
            Arg::new("skip_tools_install")
                .long("skip-tools-install")
                .takes_value(false)
                .conflicts_with("force_tools_install")
                .help(
                    "Skip downloading and installing platform-tools, assuming they are properly \
                     mounted",
                ),
        )
        .arg(
            Arg::new("no_rustup_override")
                .long("no-rustup-override")
                .takes_value(false)
                .conflicts_with("force_tools_install")
                .help(
                    "Do not use rustup to manage the toolchain. By default, cargo-build-sbf \
                     invokes rustup to find the Solana rustc using a `+solana` toolchain \
                     override. This flag disables that behavior.",
                ),
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
            Arg::new("quiet")
                .short('q')
                .long("quiet")
                .takes_value(false)
                .help("Do not print cargo log messages"),
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
                .possible_values(["v0", "v1", "v2", "v3", "v4"])
                .default_value("v0")
                .help("Build for the given target architecture"),
        )
        .arg(
            Arg::new("optimize_size")
                .long("optimize-size")
                .takes_value(false)
                .help(
                    "Optimize program for size. This option may reduce program size, potentially \
                     increasing CU consumption.",
                ),
        )
        .arg(Arg::new("lto").long("lto").takes_value(false).help(
            "Enable Link-Time Optimization (LTO) for all crates being built. This option may \
             decrease program size and CU consumption. The default option is LTO disabled, as one \
             may get mixed results with it.",
        ))
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
        quiet: matches.is_present("quiet"),
        workspace: matches.is_present("workspace"),
        jobs: matches.value_of_t("jobs").ok(),
        arch: matches.value_of("arch").unwrap(),
        optimize_size: matches.is_present("optimize_size"),
        lto: matches.is_present("lto"),
        install_only: matches.is_present("install_only"),
    };
    let manifest_path: Option<PathBuf> = matches.value_of_t("manifest_path").ok();
    if config.verbose {
        debug!("{config:?}");
        debug!("manifest_path: {manifest_path:?}");
    }

    if config.install_only {
        let platform_tools_version = validate_platform_tools_version(
            config
                .platform_tools_version
                .unwrap_or(DEFAULT_PLATFORM_TOOLS_VERSION),
            DEFAULT_PLATFORM_TOOLS_VERSION,
        );
        install_tools(&config, &platform_tools_version, true);
        return;
    }

    build_solana(config, manifest_path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_version_string_valid_versions() {
        // Test valid versions that should pass validation
        assert!(is_version_string("1.2.3").is_ok());
        assert!(is_version_string("v2.1.0").is_ok());
        assert!(is_version_string("1.32").is_ok());
        assert!(is_version_string("v1.32").is_ok());
        assert!(is_version_string("0.1").is_ok());
        assert!(is_version_string("v0.1").is_ok());
        assert!(is_version_string("10.20.30").is_ok());
        assert!(is_version_string("v10.20.30").is_ok());
    }

    #[test]
    fn test_is_version_string_invalid_versions() {
        // Test invalid versions that should fail validation
        assert!(is_version_string("1.2.3abc").is_err());
        assert!(is_version_string("v2.1.0-extra").is_err());
        assert!(is_version_string("abc1.2.3").is_err());
        assert!(is_version_string("1").is_err());
        assert!(is_version_string("v1").is_err());
        assert!(is_version_string("1.2.3.4.5").is_err());
        assert!(is_version_string("").is_err());
        assert!(is_version_string("v").is_err());
        assert!(is_version_string("1.").is_err());
        assert!(is_version_string("v1.").is_err());
        assert!(is_version_string(".1.2").is_err());
        assert!(is_version_string("1.2.3-beta").is_err());
        assert!(is_version_string("v1.2.3+build").is_err());
    }

    #[test]
    fn test_is_version_string_error_message() {
        // Test that error message is descriptive
        let result = is_version_string("invalid");
        assert!(result.is_err());
        let error_msg = result.unwrap_err();
        assert!(error_msg.contains("version string may start with 'v'"));
        assert!(error_msg.contains("major and minor version numbers"));
        assert!(error_msg.contains("separated by a dot"));
    }
}
