use {
    crate::{postprocess_dump, rust_target_triple, spawn, Config},
    log::{debug, error, info, warn},
    regex::Regex,
    solana_keypair::{write_keypair_file, Keypair},
    std::{
        collections::HashSet,
        fs::{self, File},
        io::{BufRead, BufReader},
        path::Path,
        process::exit,
    },
};

pub(crate) fn post_process(
    config: &Config,
    package: &cargo_metadata::Package,
    target_directory: &Path,
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

    let sbf_out_dir = config
        .sbf_out_dir
        .as_ref()
        .cloned()
        .unwrap_or_else(|| target_directory.join("deploy"));
    let target_triple = rust_target_triple(config);
    let target_build_directory = target_directory.join(&target_triple).join("release");

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
