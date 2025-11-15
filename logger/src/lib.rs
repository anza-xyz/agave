//! The `logger` module configures `env_logger`
#![cfg(feature = "agave-unstable-api")]
use std::{
    env,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, RwLock},
    thread::JoinHandle,
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

    fn flush(&self) {}
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
            .build();
    replace_logger(logger);
}

// Configures logging with a default filter if RUST_LOG is not set
pub fn setup_with_default(filter: &str) {
    let logger = env_logger::Builder::from_env(env_logger::Env::new().default_filter_or(filter))
        .format_timestamp_nanos()
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

// Configures file logging with a default filter if RUST_LOG is not set
#[cfg(not(unix))]
fn setup_file_with_default_filter(logfile: &Path) {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logfile)
        .unwrap();

    let logger =
        env_logger::Builder::from_env(env_logger::Env::new().default_filter_or(DEFAULT_FILTER))
            .format_timestamp_nanos()
            .target(env_logger::Target::Pipe(Box::new(file)))
            .build();
    replace_logger(logger);
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
    // Debugging panics is easier with a backtrace
    if env::var_os("RUST_BACKTRACE").is_none() {
        env::set_var("RUST_BACKTRACE", "1")
    }

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

// Redirect stderr to a file with support for logrotate by sending a SIGUSR1 to the process.
//
// Upon success, future `log` macros and `eprintln!()` can be found in the specified log file.
pub fn redirect_stderr_to_file(logfile: Option<PathBuf>) -> Option<JoinHandle<()>> {
    // Default to RUST_BACKTRACE=1 for more informative validator logs
    if env::var_os("RUST_BACKTRACE").is_none() {
        env::set_var("RUST_BACKTRACE", "1")
    }

    match logfile {
        None => {
            setup_with_default_filter();
            None
        }
        Some(logfile) => {
            #[cfg(unix)]
            {
                use log::info;
                let mut signals =
                    signal_hook::iterator::Signals::new([signal_hook::consts::SIGUSR1])
                        .unwrap_or_else(|err| {
                            eprintln!("Unable to register SIGUSR1 handler: {err:?}");
                            std::process::exit(1);
                        });

                setup_with_default_filter();
                redirect_stderr(&logfile);
                Some(
                    std::thread::Builder::new()
                        .name("solSigUsr1".into())
                        .spawn(move || {
                            for signal in signals.forever() {
                                info!(
                                    "received SIGUSR1 ({signal}), reopening log file: {logfile:?}",
                                );
                                redirect_stderr(&logfile);
                            }
                        })
                        .unwrap(),
                )
            }
            #[cfg(not(unix))]
            {
                println!("logrotate is not supported on this platform");
                setup_file_with_default_filter(&logfile);
                None
            }
        }
    }
}
