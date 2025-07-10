use {
    crate::{spawn, utils::rust_target_triple, Config},
    log::{debug, error, info, warn},
    regex::Regex,
    solana_keypair::{write_keypair_file, Keypair},
    std::{
        collections::{HashMap, HashSet},
        fs::{self, File},
        io::{BufRead, BufReader, BufWriter, Write},
        path::Path,
        process::exit,
        str::FromStr,
    },
};

pub(crate) fn post_process(config: &Config, target_directory: &Path, program_name: Option<String>) {
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

        #[cfg(windows)]
        let llvm_bin = config
            .sbf_sdk
            .join("dependencies")
            .join("platform-tools")
            .join("llvm")
            .join("bin");

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
                debug!("{output}");
            }
        }

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
                debug!("{output}");
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
