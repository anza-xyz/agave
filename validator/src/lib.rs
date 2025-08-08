#![allow(clippy::arithmetic_side_effects)]
#[cfg(not(target_os = "windows"))]
use signal_hook::{consts::SIGTERM, consts::SIGUSR1, iterator::Signals};
pub use solana_test_validator as test_validator;
use {
    console::style,
    fd_lock::{RwLock, RwLockWriteGuard},
    indicatif::{ProgressDrawTarget, ProgressStyle},
    solana_validator_exit::Exit,
    std::{
        borrow::Cow,
        env,
        fmt::Display,
        fs::{File, OpenOptions},
        path::Path,
        process::exit,
        sync::Arc,
        thread::JoinHandle,
        time::Duration,
    },
};

pub mod admin_rpc_service;
pub mod bootstrap;
pub mod cli;
pub mod commands;
pub mod dashboard;

#[cfg(unix)]
fn redirect_stderr(filename: &str) {
    use std::os::unix::io::AsRawFd;
    match OpenOptions::new().create(true).append(true).open(filename) {
        Ok(file) => unsafe {
            libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO);
        },
        Err(err) => eprintln!("Unable to open {filename}: {err}"),
    }
}

#[cfg_attr(not(unix), allow(unused_variables))]
fn create_signal_handler_thread(
    logfile: Option<String>,
    validator_exit: Arc<std::sync::RwLock<Exit>>,
) -> Option<JoinHandle<()>> {
    // Default to RUST_BACKTRACE=1 for more informative validator logs
    if env::var_os("RUST_BACKTRACE").is_none() {
        env::set_var("RUST_BACKTRACE", "1")
    }

    #[cfg(unix)]
    {
        solana_logger::setup_with_default_filter();
        if let Some(ref logfile) = logfile {
            redirect_stderr(logfile);
        }

        use log::{info, warn};
        let mut signals = Signals::new([SIGTERM, SIGUSR1]).unwrap_or_else(|err| {
            eprintln!("Unable to register SIGUSR1 handler: {err:?}");
            exit(1);
        });

        std::thread::Builder::new()
            .name("solSigHandler".into())
            .spawn(move || {
                for signal in signals.forever() {
                    match signal {
                        SIGTERM => {
                            info!("Received SIGTERM ({}). Initiating graceful exit.", signal);
                            validator_exit.write().unwrap().exit();
                        }
                        SIGUSR1 => {
                            if let Some(ref logfile) = logfile {
                                info!(
                                    "Received SIGUSR1 ({}), reopening log file: {:?}",
                                    signal, logfile
                                );
                                redirect_stderr(logfile);
                            }
                        }
                        s => warn!("Received unknown signal: {}", s),
                    }
                }
            })
            .ok()
    }
    #[cfg(not(unix))]
    {
        println!("Signal handling is not supported on this platform.");
        match logfile {
            Some(logfile) => {
                solana_logger::setup_file_with_default(&logfile, solana_logger::DEFAULT_FILTER);
            }
            None => {
                solana_logger::setup_with_default_filter();
            }
        }
        None
    }
}

pub fn format_name_value(name: &str, value: &str) -> String {
    format!("{} {}", style(name).bold(), value)
}
/// Pretty print a "name value"
pub fn println_name_value(name: &str, value: &str) {
    println!("{}", format_name_value(name, value));
}

/// Creates a new process bar for processing that will take an unknown amount of time
pub fn new_spinner_progress_bar() -> ProgressBar {
    let progress_bar = indicatif::ProgressBar::new(42);
    progress_bar.set_draw_target(ProgressDrawTarget::stdout());
    progress_bar.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.green} {wide_msg}")
            .expect("ProgresStyle::template direct input to be correct"),
    );
    progress_bar.enable_steady_tick(Duration::from_millis(100));

    ProgressBar {
        progress_bar,
        is_term: console::Term::stdout().is_term(),
    }
}

pub struct ProgressBar {
    progress_bar: indicatif::ProgressBar,
    is_term: bool,
}

impl ProgressBar {
    pub fn set_message<T: Into<Cow<'static, str>> + Display>(&self, msg: T) {
        if self.is_term {
            self.progress_bar.set_message(msg);
        } else {
            println!("{msg}");
        }
    }

    pub fn println<I: AsRef<str>>(&self, msg: I) {
        self.progress_bar.println(msg);
    }

    pub fn abandon_with_message<T: Into<Cow<'static, str>> + Display>(&self, msg: T) {
        if self.is_term {
            self.progress_bar.abandon_with_message(msg);
        } else {
            println!("{msg}");
        }
    }
}

pub fn ledger_lockfile(ledger_path: &Path) -> RwLock<File> {
    let lockfile = ledger_path.join("ledger.lock");
    fd_lock::RwLock::new(
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(lockfile)
            .unwrap(),
    )
}

pub fn lock_ledger<'lock>(
    ledger_path: &Path,
    ledger_lockfile: &'lock mut RwLock<File>,
) -> RwLockWriteGuard<'lock, File> {
    ledger_lockfile.try_write().unwrap_or_else(|_| {
        println!(
            "Error: Unable to lock {} directory. Check if another validator is running",
            ledger_path.display()
        );
        exit(1);
    })
}
