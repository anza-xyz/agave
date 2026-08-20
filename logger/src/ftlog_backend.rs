//! Prototype backend: `ftlog` formats and writes log messages on its own thread,
//! leaving the calling thread with a filter check and a channel send.
//!
//! `ftlog`'s own target filtering happens on the log thread and only understands
//! level-per-prefix, so filtering is kept here on the calling thread with
//! `env_filter` instead: same filter syntax as `env_logger`, and records that are
//! filtered out never allocate or reach the channel.
use {
    ftlog::FtLogFormat,
    log::{Level, LevelFilter, Log},
    std::{
        borrow::Cow,
        fmt::{self, Display},
        io::Write,
        sync::RwLock,
    },
};

/// Messages that may be queued for the log thread.
const QUEUE_CAPACITY: usize = 100_000;

/// What to do once `QUEUE_CAPACITY` is reached: block the calling thread, or drop
/// the message and report the number dropped to stderr every few seconds.
///
/// A single log thread sustains roughly 800k lines/s. Above that, dropping keeps
/// callers fast but loses most messages, while blocking makes callers slower than
/// the synchronous `env_logger` backend, since one thread then does all formatting.
const BLOCK_WHEN_FULL: bool = false;

/// `env_logger`-compatible timestamp, e.g. `[2026-08-19T12:00:00.123456789Z`. The
/// leading bracket is escaped as `[[`; `ftlog` emits the timestamp itself and
/// appends its queueing delay to it, so the bracket has to be opened here.
const TIME_FORMAT: &str = "[[[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:9]Z";

static ACTIVE: RwLock<Option<Backend>> = RwLock::new(None);

struct Backend {
    filter: env_filter::Filter,
    logger: ftlog::Logger,
}

/// Installed once; forwards to whatever backend is current so that the filter can
/// be reconfigured after logging has started.
struct LoggerShim {}

impl Log for LoggerShim {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        let active = ACTIVE.read().unwrap();
        active
            .as_ref()
            .is_some_and(|backend| backend.filter.enabled(metadata))
    }

    fn log(&self, record: &log::Record) {
        let active = ACTIVE.read().unwrap();
        if let Some(backend) = active.as_ref()
            && backend.filter.matches(record)
        {
            backend.logger.log(record);
        }
    }

    fn flush(&self) {
        if let Some(backend) = ACTIVE.read().unwrap().as_ref() {
            backend.logger.flush();
        }
    }
}

pub(crate) fn install(env_var: &str, fallback: &str) {
    replace_logger(filter(env_var, fallback), Box::new(std::io::stderr()));
}

#[cfg(not(unix))]
pub(crate) fn install_to_file(env_var: &str, fallback: &str, logfile: &std::path::Path) {
    replace_logger(
        filter(env_var, fallback),
        Box::new(ftlog::appender::FileAppender::new(logfile)),
    );
}

fn filter(env_var: &str, fallback: &str) -> env_filter::Filter {
    let spec = std::env::var(env_var).unwrap_or_else(|_| fallback.to_string());
    env_filter::Builder::new().parse(&spec).build()
}

fn replace_logger(filter: env_filter::Filter, root: Box<dyn Write + Send>) {
    // Built before the lock is taken: `build` spawns the log thread, which logs
    // through the shim and would deadlock against a held write lock.
    let logger = ftlog::builder()
        .format(Formatter {})
        .time_format(time_format())
        .root(root)
        .utc()
        // Levels are enforced by `filter` on the calling thread, so nothing that
        // reaches the log thread may be dropped for being too verbose.
        .max_log_level(LevelFilter::Trace)
        .bounded(QUEUE_CAPACITY, BLOCK_WHEN_FULL)
        .print_omitted_count(true)
        .build()
        .expect("failed to start ftlog log thread");

    log::set_max_level(filter.filter());
    let previous = ACTIVE.write().unwrap().replace(Backend { filter, logger });
    let _ = log::set_boxed_logger(Box::new(LoggerShim {}));

    // Outside the lock: flushing waits on the log thread, which logs on write
    // errors. Dropping the logger afterwards lets its thread drain and exit.
    if let Some(previous) = previous {
        previous.logger.flush();
    }
}

fn time_format() -> time::format_description::OwnedFormatItem {
    time::format_description::parse_owned::<1>(TIME_FORMAT).expect("valid time format")
}

/// Reproduces `env_logger`'s default format. With `ftlog`'s timestamp and
/// queueing delay prefixed, a line reads:
/// `[2026-08-19T12:00:00.123456789Z 0ms INFO  solana_metrics] message`
struct Formatter {}

impl FtLogFormat for Formatter {
    fn msg(&self, record: &log::Record) -> Box<dyn Send + Sync + Display> {
        Box::new(Message {
            level: record.level(),
            // `Record` hands out a borrowed target; the common case is the static
            // module path, which needs no copy.
            target: match record.module_path_static() {
                Some(module_path) if module_path == record.target() => Cow::Borrowed(module_path),
                _ => Cow::Owned(record.target().to_string()),
            },
            // Rendering is the log thread's job, but only a literal message can be
            // handed over as is.
            args: match record.args().as_str() {
                Some(literal) => Cow::Borrowed(literal),
                None => Cow::Owned(record.args().to_string()),
            },
        })
    }
}

struct Message {
    level: Level,
    target: Cow<'static, str>,
    args: Cow<'static, str>,
}

impl Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:<5} {}] {}", self.level, self.target, self.args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_time_format_is_valid() {
        time_format();
    }

    #[test]
    fn test_message_format() {
        let message = Message {
            level: Level::Info,
            target: Cow::Borrowed("solana_metrics"),
            args: Cow::Borrowed("datapoint"),
        };
        assert_eq!(message.to_string(), "INFO  solana_metrics] datapoint");
    }
}
