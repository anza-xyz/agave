use {
    crate::{
        spawn,
        toolchain::rust_target_triple,
        utils::{copy_file, create_directory, generate_keypair},
        Config,
    },
    log::{debug, error, info, warn},
    regex::Regex,
    std::{
        collections::{HashMap, HashSet},
        fs::{self, File},
        io::{BufRead, BufReader, BufWriter, Write},
        path::{Path, PathBuf},
        process::exit,
        str::FromStr,
    },
};

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

fn create_folders(config: &Config, deploy_folder: &PathBuf, debug_folder: &PathBuf) {
    if !deploy_folder.exists() {
        create_directory(deploy_folder);
    }

    if config.debug && !debug_folder.exists() {
        create_directory(debug_folder);
    }
}

fn strip_object(
    config: &Config,
    unstripped: &Path,
    stripped: &Path,
    #[cfg(windows)] llvm_bin: &Path,
) {
    #[cfg(windows)]
    let output = spawn(
        &llvm_bin.join("llvm-objcopy"),
        [
            "--strip-all".as_ref(),
            unstripped.as_os_str(),
            stripped.as_os_str(),
        ],
        config.generate_child_script_on_failure,
    );
    #[cfg(not(windows))]
    let output = spawn(
        &config.sbf_sdk.join("scripts").join("strip.sh"),
        [
            unstripped.as_os_str(),
            stripped.as_os_str(),
            "--strip-all".as_ref(),
        ],
        config.generate_child_script_on_failure,
    );
    if config.verbose {
        debug!("{output}");
    }
}

fn generate_debug_objects(
    config: &Config,
    sbf_debug_dir: &Path,
    finalized_program: &str,
    program_unstripped_so: &Path,
    deploy_keypair: &Path,
    debug_keypair: &PathBuf,
    program_name: &str,
    #[cfg(windows)] llvm_bin: &Path,
) -> PathBuf {
    // debug objects
    let program_debug = sbf_debug_dir.join(format!("{program_name}.so.debug"));
    let program_debug_stripped = sbf_debug_dir.join(finalized_program);

    if file_older_or_missing(program_unstripped_so, &program_debug_stripped) {
        strip_object(
            config,
            program_unstripped_so,
            &program_debug_stripped,
            #[cfg(windows)]
            llvm_bin,
        );
    }

    if file_older_or_missing(program_unstripped_so, &program_debug) {
        copy_file(program_unstripped_so, &program_debug);
    }

    if !debug_keypair.exists() {
        if deploy_keypair.exists() {
            copy_file(deploy_keypair, debug_keypair);
        } else {
            generate_keypair(debug_keypair);
        }
    }

    program_debug_stripped
}

fn generate_release_objects(
    config: &Config,
    sbf_out_dir: &Path,
    program_unstripped_so: &Path,
    deploy_keypair: &PathBuf,
    debug_keypair: &Path,
    program_name: &str,
    #[cfg(windows)] llvm_bin: &Path,
) -> PathBuf {
    let program_so = sbf_out_dir.join(format!("{program_name}.so"));

    if file_older_or_missing(program_unstripped_so, &program_so) {
        strip_object(
            config,
            program_unstripped_so,
            &program_so,
            #[cfg(windows)]
            llvm_bin,
        );
    }

    if !deploy_keypair.exists() {
        if debug_keypair.exists() {
            copy_file(debug_keypair, deploy_keypair);
        } else {
            generate_keypair(deploy_keypair);
        }
    }

    program_so
}

pub(crate) fn post_process(config: &Config, target_directory: &Path, program_name: Option<String>) {
    let sbf_out_dir = config
        .sbf_out_dir
        .as_ref()
        .cloned()
        .unwrap_or_else(|| target_directory.join("deploy"));
    let target_triple = rust_target_triple(config);
    let sbf_debug_dir = sbf_out_dir.join("debug");

    create_folders(config, &sbf_out_dir, &sbf_debug_dir);

    let target_build_directory = if config.debug {
        target_directory.join(&target_triple).join("debug")
    } else {
        target_directory.join(&target_triple).join("release")
    };

    if let Some(program_name) = program_name {
        let keypair_name = format!("{program_name}-keypair.json");
        let deploy_keypair = sbf_out_dir.join(keypair_name.clone());
        let debug_keypair = sbf_debug_dir.join(keypair_name);

        let finalized_program_file = format!("{program_name}.so");

        let program_unstripped_so = target_build_directory.join(finalized_program_file.clone());
        let program_dump = sbf_out_dir.join(format!("{program_name}-dump.txt"));

        #[cfg(windows)]
        let llvm_bin = config
            .sbf_sdk
            .join("dependencies")
            .join("platform-tools")
            .join("llvm")
            .join("bin");

        let program_so = if config.debug {
            generate_debug_objects(
                config,
                &sbf_debug_dir,
                &finalized_program_file,
                &program_unstripped_so,
                &deploy_keypair,
                &debug_keypair,
                &program_name,
                #[cfg(windows)]
                &llvm_bin,
            )
        } else {
            generate_release_objects(
                config,
                &sbf_out_dir,
                &program_unstripped_so,
                &deploy_keypair,
                &debug_keypair,
                &program_name,
                #[cfg(windows)]
                &llvm_bin,
            )
        };

        if config.dump && file_older_or_missing(&program_unstripped_so, &program_dump) {
            let dump_script = config.sbf_sdk.join("scripts").join("dump.sh");
            #[cfg(windows)]
            {
                error!(
                    "Using Bash scripts from within a program is not supported on Windows, \
                     skipping `--dump`."
                );
                error!(
                    "Please run \"{} {} {}\" from a Bash-supporting shell, then re-run this \
                     command to see the processed program dump.",
                    &dump_script.display(),
                    &program_unstripped_so.display(),
                    &program_dump.display()
                );
            }
            #[cfg(not(windows))]
            {
                let output = spawn(
                    &dump_script,
                    [&program_unstripped_so, &program_dump],
                    config.generate_child_script_on_failure,
                );
                if config.verbose {
                    debug!("{output}");
                }
            }
            postprocess_dump(&program_dump);
        }

        if config.arch != "v3" {
            // SBPFv3 shall not have any undefined syscall.
            check_undefined_symbols(config, &program_so);
        }

        if !config.debug {
            info!("To deploy this program:");
            info!("  $ solana program deploy {}", program_so.display());
            info!("The program address will default to this keypair (override with --program-id):");
            info!("  {}", deploy_keypair.display());
        }
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
        debug!("{output}");
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
            "The following functions are undefined and not known syscalls {unresolved_symbols:?}."
        );
        warn!("         Calling them will trigger a run-time error.");
    }
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
