use {
    regex::Regex,
    std::{
        fs::File,
        io::{prelude::*, BufReader, BufWriter},
        path::PathBuf,
        str,
    },
};

/**
 * Extract a list of registered syscall names and save it in a file
 * for distribution with the SDK.  This file is read by cargo-build-sbf
 * to verify undefined symbols in a .so module that cargo-build-sbf has built.
 */
fn main() {
    let syscalls_rs_name = "../src/lib.rs";
    let syscalls_txt_name = "../../platform-tools-sdk/sbf/syscalls.txt";
    println!("cargo::rerun-if-changed={syscalls_rs_name}");
    println!("cargo::rerun-if-changed={syscalls_txt_name}");
    println!("cargo::rerun-if-changed=build.rs");

    let syscalls_rs_path = PathBuf::from(syscalls_rs_name);
    let syscalls_txt_path = PathBuf::from(syscalls_txt_name);
    println!(
        "cargo::warning=(not a warning) Generating {1} from {0}",
        syscalls_rs_path.display(),
        syscalls_txt_path.display(),
    );

    let old_num_syscalls = File::open(&syscalls_txt_path)
        .map(|file| {
            let reader = BufReader::new(file);
            reader.lines().count()
        })
        .unwrap_or(0);

    let mut file = match File::open(&syscalls_rs_path) {
        Ok(x) => x,
        Err(err) => panic!("Failed to open {}: {}", syscalls_rs_path.display(), err),
    };
    let mut text = vec![];
    file.read_to_end(&mut text).unwrap();
    let text = str::from_utf8(&text).unwrap();
    let sysc_re = Regex::new(r#"register_function\([[:space:]]*"([^"]+)","#).unwrap();
    let feature_gate_syscall_re =
        Regex::new(r#"register_feature_gated_function!\([^"]+"([^"]+)","#).unwrap();
    let new_num_syscalls = sysc_re
        .captures_iter(text)
        .chain(feature_gate_syscall_re.captures_iter(text))
        .count();
    if new_num_syscalls < old_num_syscalls {
        println!(
            "cargo:error=Number of syscalls reduced from {old_num_syscalls} to \
             {new_num_syscalls}, parsing logic in build.rs likely needs to be fixed."
        );
        std::process::exit(1);
    }

    let txt_file = match File::create(&syscalls_txt_path) {
        Ok(x) => x,
        Err(err) => panic!("Failed to create {}: {}", syscalls_txt_path.display(), err),
    };
    let mut txt_out = BufWriter::new(txt_file);
    for caps in sysc_re
        .captures_iter(text)
        .chain(feature_gate_syscall_re.captures_iter(text))
    {
        let name = caps[1].to_string();
        writeln!(txt_out, "{name}").unwrap();
    }
}
