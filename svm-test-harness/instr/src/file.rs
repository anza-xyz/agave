//! Module for loading files from local filesystem.
//!
//! When compiling a Solana program with `cargo build-sbf`, the environment
//! variable `SBF_OUT_DIR` defaults to `target/deploy` and the compiled program
//! ELF file is written to `target/deploy/program_name.so`.
//!
//! As a result, the default search paths for ELF files are:
//!
//! * `tests/fixtures`
//! * `BPF_OUT_DIR`
//! * `SBF_OUT_DIR`
//! * The current working directory
//!
//! Since these functions are intended for the local filesystem and for testing
//! purposes, they will panic if the file is not found or if there is an
//! error reading the file.

use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

fn default_shared_object_dirs() -> Vec<PathBuf> {
    let mut search_path = vec![PathBuf::from("tests/fixtures")];

    if let Ok(bpf_out_dir) = std::env::var("BPF_OUT_DIR") {
        search_path.push(PathBuf::from(bpf_out_dir));
    }

    if let Ok(sbf_out_dir) = std::env::var("SBF_OUT_DIR") {
        search_path.push(PathBuf::from(sbf_out_dir));
    }

    if let Ok(dir) = std::env::current_dir() {
        search_path.push(dir);
    }

    search_path
}

fn find_file(filename: &str) -> Option<PathBuf> {
    for dir in default_shared_object_dirs() {
        let candidate = dir.join(filename);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Read the contents of a file into a `Vec<u8>`.
pub fn read_file<P: AsRef<Path>>(path: P) -> Vec<u8> {
    let path = path.as_ref();
    let mut file = File::open(path)
        .unwrap_or_else(|e| panic!("Failed to open file {}: {}", path.display(), e));

    let mut file_data = Vec::new();
    file.read_to_end(&mut file_data)
        .unwrap_or_else(|e| panic!("Failed to read file {}: {}", path.display(), e));
    file_data
}

/// Load a program ELF file from the local filesystem by program name.
///
/// The program ELF file is expected to be located in one of the default search
/// paths:
///
/// * `tests/fixtures`
/// * `BPF_OUT_DIR`
/// * `SBF_OUT_DIR`
/// * The current working directory
///
/// The name of the program ELF file is expected to be `{program_name}.so`.
pub fn load_program_elf(program_name: &str) -> Vec<u8> {
    let file_name = format!("{program_name}.so");
    let program_file = find_file(&file_name)
        .unwrap_or_else(|| panic!("Failed to find program ELF file: {file_name}"));
    read_file(program_file)
}
