use {
    itertools::Itertools,
    log::{error, info},
    solana_keypair::{write_keypair_file, Keypair},
    std::{
        env,
        ffi::OsStr,
        fs::File,
        io::{BufWriter, Write},
        path::{Path, PathBuf},
        process::{exit, Command, Stdio},
    },
};

pub(crate) fn spawn<I, S>(program: &Path, args: I, generate_child_script_on_failure: bool) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args = Vec::from_iter(args);
    let msg = args
        .iter()
        .map(|arg| arg.as_ref().to_str().unwrap_or("?"))
        .join(" ");
    info!("spawn: {program:?} {msg}");

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
        writeln!(out, "{msg}").unwrap();
        out.flush().unwrap();
        error!("To rerun the failed command for debugging use {script_name}");
        exit(1);
    }
    output
        .stdout
        .as_slice()
        .iter()
        .map(|&c| c as char)
        .collect::<String>()
}

pub(crate) fn create_directory(path: &PathBuf) {
    std::fs::create_dir_all(path).unwrap_or_else(|err| {
        error!("Failed create folder: {err}");
        exit(1);
    });
}

pub(crate) fn copy_file(from: &Path, to: &Path) {
    std::fs::copy(from, to).unwrap_or_else(|err| {
        error!("Failed to copy file: {err}");
        exit(1);
    });
}

pub(crate) fn generate_keypair(path: &PathBuf) {
    write_keypair_file(&Keypair::new(), path).unwrap_or_else(|err| {
        error!("Unable to create {}: {err}", path.display());
        exit(1);
    });
}
