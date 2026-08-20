#![cfg(feature = "agave-unstable-api")]
//! The `logger` module configures the process-wide `log` backend.
//!
//! Two interchangeable backends: `env_logger` by default, or `ftlog` with the
//! `ftlog` feature. Both take `env_logger` filter syntax and write to stderr, so
//! `redirect_stderr` and the `SIGUSR1` log rotation it implements work with either.
//!
//! To build a binary against the `ftlog` backend without editing manifests:
//! `cargo build -p agave-validator --features agave-logger/ftlog`
use std::path::{Path, PathBuf};

#[cfg_attr(feature = "ftlog", path = "ftlog_backend.rs")]
#[cfg_attr(not(feature = "ftlog"), path = "env_logger_backend.rs")]
mod backend;

pub const DEFAULT_FILTER: &str = "solana=info,agave=info";

// Configures logging with a specific filter overriding RUST_LOG.  _RUST_LOG is used instead
// so if set it takes precedence.
// May be called at any time to re-configure the log filter
pub fn setup_with(filter: &str) {
    backend::install("_RUST_LOG", filter);
}

// Configures logging with a default filter if RUST_LOG is not set
pub fn setup_with_default(filter: &str) {
    backend::install("RUST_LOG", filter);
}

// Configures logging with the `DEFAULT_FILTER` if RUST_LOG is not set
pub fn setup_with_default_filter() {
    setup_with_default(DEFAULT_FILTER);
}

// Configures logging with the default filter "error" if RUST_LOG is not set
pub fn setup() {
    setup_with_default("error");
}

// Waits for pending log messages to be written out. With the `ftlog` backend
// messages are written by a background thread and are otherwise lost when the
// process exits without flushing.
pub fn flush() {
    log::logger().flush();
}

// Configures file logging with a default filter if RUST_LOG is not set
#[cfg(not(unix))]
fn setup_file_with_default_filter(logfile: &Path) {
    backend::install_to_file("RUST_LOG", DEFAULT_FILTER, logfile);
}

#[cfg(unix)]
pub fn redirect_stderr(filename: &Path) {
    use std::{fs::OpenOptions, os::unix::io::AsRawFd};
    match OpenOptions::new().create(true).append(true).open(filename) {
        Ok(file) => unsafe {
            libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO);
        },
        Err(err) => eprintln!("Unable to open {}: {err}", filename.display()),
    }
}

pub fn initialize_logging(logfile: Option<PathBuf>) {
    let Some(logfile) = logfile else {
        setup_with_default_filter();
        return;
    };

    #[cfg(unix)]
    {
        setup_with_default_filter();
        redirect_stderr(&logfile);
    }
    #[cfg(not(unix))]
    {
        setup_file_with_default_filter(&logfile);
    }
}
