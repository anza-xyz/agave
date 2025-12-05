use {
    regex::Regex,
    std::{
        fs::File,
        io::{prelude::*, BufWriter},
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
    let syscalls_rs_path = PathBuf::from("../src/lib.rs");
    let syscalls_txt_path = PathBuf::from("../../platform-tools-sdk/sbf/syscalls.txt");
    let build_sbf_syscalls_path =
        PathBuf::from("../../platform-tools-sdk/cargo-build-sbf/src/syscalls.rs");
    println!(
        "cargo:warning=(not a warning) Generating {1} and {2} from {0}",
        syscalls_rs_path.display(),
        syscalls_txt_path.display(),
        build_sbf_syscalls_path.display(),
    );

    let mut file = match File::open(&syscalls_rs_path) {
        Ok(x) => x,
        Err(err) => panic!("Failed to open {}: {}", syscalls_rs_path.display(), err),
    };
    let mut text = vec![];
    file.read_to_end(&mut text).unwrap();
    let text = str::from_utf8(&text).unwrap();
    let txt_file = match File::create(&syscalls_txt_path) {
        Ok(x) => x,
        Err(err) => panic!("Failed to create {}: {}", syscalls_txt_path.display(), err),
    };
    let mut txt_out = BufWriter::new(txt_file);
    let rs_file = match File::create(&build_sbf_syscalls_path) {
        Ok(x) => x,
        Err(err) => panic!("Failed to create {}: {}", syscalls_txt_path.display(), err),
    };
    let mut rs_out = BufWriter::new(rs_file);
    writeln!(rs_out, "pub(crate) const SYSCALLS: &[&str] = &[").unwrap();
    let sysc_re = Regex::new(r#"register_function\([[:space:]]*"([^"]+)","#).unwrap();
    for caps in sysc_re.captures_iter(text) {
        let name = caps[1].to_string();
        writeln!(txt_out, "{name}").unwrap();
        writeln!(rs_out, "    \"{name}\",").unwrap();
    }
    let feature_gate_syscall_re =
        Regex::new(r#"register_feature_gated_function!\([^"]+"([^"]+)","#).unwrap();
    for caps in feature_gate_syscall_re.captures_iter(text) {
        let name = caps[1].to_string();
        writeln!(txt_out, "{name}").unwrap();
        writeln!(rs_out, "    \"{name}\",").unwrap();
    }
    writeln!(rs_out, "];").unwrap();
}
