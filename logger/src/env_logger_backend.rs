//! Default backend: `env_logger` formats and writes on the calling thread.
use {
    log::Log,
    std::sync::{LazyLock, RwLock},
};

static LOGGER: LazyLock<RwLock<env_logger::Logger>> =
    LazyLock::new(|| RwLock::new(env_logger::Logger::from_default_env()));

/// Installed once; forwards to whatever logger is current so that the filter can
/// be reconfigured after logging has started.
struct LoggerShim {}

impl Log for LoggerShim {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        LOGGER.read().unwrap().enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        LOGGER.read().unwrap().log(record);
    }

    fn flush(&self) {
        LOGGER.read().unwrap().flush();
    }
}

pub(crate) fn install(env_var: &str, fallback: &str) {
    replace_logger(builder(env_var, fallback).build());
}

#[cfg(not(unix))]
pub(crate) fn install_to_file(env_var: &str, fallback: &str, logfile: &std::path::Path) {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logfile)
        .unwrap();

    let logger = builder(env_var, fallback)
        .target(env_logger::Target::Pipe(Box::new(file)))
        .build();
    replace_logger(logger);
}

fn builder(env_var: &str, fallback: &str) -> env_logger::Builder {
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::new().filter_or(env_var, fallback));
    builder.format_timestamp_nanos();
    builder
}

fn replace_logger(logger: env_logger::Logger) {
    log::set_max_level(logger.filter());
    *LOGGER.write().unwrap() = logger;
    let _ = log::set_boxed_logger(Box::new(LoggerShim {}));
}
