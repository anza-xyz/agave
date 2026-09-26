#![cfg(feature = "agave-unstable-api")]
//! The `logger` module configures `env_logger`
//!
//! Records are filtered and formatted on the calling thread, then written to the log by a
//! background thread. See [`writer`] for why, and for the guarantees that buys.
mod writer;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, RwLock},
};

static LOGGER: LazyLock<Arc<RwLock<env_logger::Logger>>> =
    LazyLock::new(|| Arc::new(RwLock::new(env_logger::Logger::from_default_env())));

pub const DEFAULT_FILTER: &str = "solana=info,agave=info";

struct LoggerShim {}

impl log::Log for LoggerShim {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        LOGGER.read().unwrap().enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        LOGGER.read().unwrap().log(record);
    }

    /// Never weaken this back to a no-op. Records are written by a background thread, so this is
    /// the only barrier callers have; without it every `process::exit` silently truncates the log
    /// at whatever was still queued. No test catches its removal: the writer normally drains
    /// faster than a test can reach its assertions, so a stubbed-out flush still looks green.
    fn flush(&self) {
        writer::flush();
    }
}

fn replace_logger(logger: env_logger::Logger) {
    log::set_max_level(logger.filter());
    *LOGGER.write().unwrap() = logger;
    let _ = log::set_boxed_logger(Box::new(LoggerShim {}));
}

// Configures logging with a specific filter overriding RUST_LOG.  _RUST_LOG is used instead
// so if set it takes precedence.
// May be called at any time to re-configure the log filter
pub fn setup_with(filter: &str) {
    let logger =
        env_logger::Builder::from_env(env_logger::Env::new().filter_or("_RUST_LOG", filter))
            .format_timestamp_nanos()
            .target(writer::target())
            .build();
    replace_logger(logger);
}

// Configures logging with a default filter if RUST_LOG is not set
pub fn setup_with_default(filter: &str) {
    let logger = env_logger::Builder::from_env(env_logger::Env::new().default_filter_or(filter))
        .format_timestamp_nanos()
        .target(writer::target())
        .build();
    replace_logger(logger);
}

// Configures logging with the `DEFAULT_FILTER` if RUST_LOG is not set
pub fn setup_with_default_filter() {
    setup_with_default(DEFAULT_FILTER);
}

// Configures logging with the default filter "error" if RUST_LOG is not set
pub fn setup() {
    setup_with_default("error");
}

#[cfg(unix)]
fn redirect_stderr(filename: &Path) {
    use std::{fs::OpenOptions, os::unix::io::AsRawFd};
    match OpenOptions::new().create(true).append(true).open(filename) {
        Ok(file) => unsafe {
            libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO);
        },
        Err(err) => eprintln!("Unable to open {}: {err}", filename.display()),
    }
}

pub fn initialize_logging(logfile: Option<PathBuf>) {
    setup_with_default_filter();
    let Some(logfile) = logfile else {
        return;
    };
    point_at_logfile(&logfile);
}

/// Reopens the log file, for logrotate.
pub fn reopen(logfile: &Path) {
    // Drain records queued for the old file before either handle moves off it.
    writer::flush();
    point_at_logfile(logfile);
}

fn point_at_logfile(logfile: &Path) {
    // Point fd 2 at the log file too, so output from code that writes to stderr directly (C
    // libraries, the runtime's own abort messages) is captured alongside log records. Both
    // handles are opened `O_APPEND`, so their writes interleave without tearing.
    #[cfg(unix)]
    redirect_stderr(logfile);
    writer::reopen(logfile);
}
