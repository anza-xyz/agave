#![cfg(feature = "agave-unstable-api")]
//! The `logger` module configures `env_logger`
#[cfg(not(feature = "auto-color"))]
use std::io::{self, Write};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, RwLock},
};

static LOGGER: LazyLock<Arc<RwLock<env_logger::Logger>>> = LazyLock::new(|| {
    Arc::new(RwLock::new(set_format_and_build(
        env_logger::Builder::from_default_env(),
    )))
});

pub const DEFAULT_FILTER: &str = "solana=info,agave=info";

fn set_format_and_build(mut builder: env_logger::Builder) -> env_logger::Logger {
    #[cfg(feature = "auto-color")]
    builder.format_timestamp_nanos();
    #[cfg(not(feature = "auto-color"))]
    builder.format(format_record);
    builder.build()
}

/// Reproduces env_logger's default format, escaping the parts controlled by the caller.
#[cfg(not(feature = "auto-color"))]
fn format_record(buf: &mut env_logger::fmt::Formatter, record: &log::Record) -> io::Result<()> {
    let timestamp = buf.timestamp_nanos();
    let level = record.level();
    let target = record.target();
    if target.is_empty() {
        write!(buf, "[{timestamp} {level:<5}] ")?;
    } else {
        write!(buf, "[{timestamp} {level:<5} ")?;
        Escaped(&mut *buf).escape(target.as_bytes())?;
        write!(buf, "] ")?;
    }
    write!(Escaped(&mut *buf), "{}", record.args())?;
    writeln!(buf)
}

/// Escapes C0 and DEL as `\xNN`, so logging untrusted input can't drive the terminal of
/// whoever reads the output later.
///
/// Only needed without `auto-color`, since anstream strips escapes itself - but it does so
/// by running a VT state machine over every record, which is why we don't just keep it on.
#[cfg(not(feature = "auto-color"))]
struct Escaped<W>(W);

#[cfg(not(feature = "auto-color"))]
impl<W: Write> Escaped<W> {
    fn escape(&mut self, bytes: &[u8]) -> io::Result<()> {
        const HEX: &[u8; 16] = b"0123456789abcdef";

        // C0 plus DEL, minus `\n` and `\t`, which the record format relies on. Stricter
        // than anstream's strip, which passes `\r`, VT and FF through.
        #[inline]
        fn is_control(byte: u8) -> bool {
            ((byte < 0x20) & (byte != b'\n') & (byte != b'\t')) | (byte == 0x7f)
        }

        // Bitwise fold rather than `any`: short-circuiting would force a scalar loop.
        if !bytes.iter().fold(false, |acc, &b| acc | is_control(b)) {
            return self.0.write_all(bytes);
        }

        // `split_inclusive` keeps the match at the end of each chunk, so the byte to escape
        // stays in reach. Only the final chunk can end on something else.
        for chunk in bytes.split_inclusive(|&byte| is_control(byte)) {
            match chunk.split_last() {
                Some((&control, head)) if is_control(control) => {
                    self.0.write_all(head)?;
                    let (hi, lo) = (usize::from(control >> 4), usize::from(control & 0xf));
                    self.0.write_all(&[b'\\', b'x', HEX[hi], HEX[lo]])?;
                }
                _ => self.0.write_all(chunk)?,
            }
        }
        Ok(())
    }

    /// Indents continuation lines, matching env_logger's default format.
    fn escape_indented(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut first = true;
        for line in bytes.split(|&byte| byte == b'\n') {
            if !first {
                self.0.write_all(b"\n    ")?;
            }
            self.escape(line)?;
            first = false;
        }
        Ok(())
    }
}

/// Only the record body arrives as `fmt::Arguments`, so only it needs rendering through
/// `Write`; the target is already a `&str`.
#[cfg(not(feature = "auto-color"))]
impl<W: Write> Write for Escaped<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_all(buf)?;
        Ok(buf.len())
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.escape_indented(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

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
    replace_logger(set_format_and_build(env_logger::Builder::from_env(
        env_logger::Env::new().filter_or("_RUST_LOG", filter),
    )));
}

// Configures logging with a default filter if RUST_LOG is not set
pub fn setup_with_default(filter: &str) {
    replace_logger(set_format_and_build(env_logger::Builder::from_env(
        env_logger::Env::new().default_filter_or(filter),
    )));
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

    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::new().default_filter_or(DEFAULT_FILTER));
    builder.target(env_logger::Target::Pipe(Box::new(file)));
    replace_logger(set_format_and_build(builder));
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

#[cfg(all(test, not(feature = "auto-color")))]
mod tests {
    use super::*;

    fn escape(input: &str) -> String {
        let mut out = Vec::new();
        Escaped(&mut out).escape(input.as_bytes()).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn escape_indented(input: &str) -> String {
        let mut out = Vec::new();
        Escaped(&mut out).escape_indented(input.as_bytes()).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn keeps_printable_and_the_whitespace_the_format_uses() {
        assert_eq!(
            escape("slot=1 hash=3xQm\tok\nnext"),
            "slot=1 hash=3xQm\tok\nnext"
        );
        assert_eq!(escape("wide ünïcödé ✓"), "wide ünïcödé ✓");
    }

    #[test]
    fn escapes_color_sequences_and_del() {
        assert_eq!(escape("\x1b[31mred\x1b[0m"), "\\x1b[31mred\\x1b[0m");
        assert_eq!(escape("bell\x07del\x7f"), "bell\\x07del\\x7f");
    }

    #[test]
    fn escapes_osc_clipboard_write() {
        assert_eq!(
            escape("\x1b]52;c;cGF5bG9hZA==\x07"),
            "\\x1b]52;c;cGF5bG9hZA==\\x07"
        );
    }

    #[test]
    fn escapes_carriage_return_that_anstream_would_pass() {
        assert_eq!(escape("real\rfake"), "real\\x0dfake");
    }

    #[test]
    fn indents_continuation_lines_and_escapes_them() {
        assert_eq!(
            escape_indented("first\n\x1b[2Jsecond"),
            "first\n    \\x1b[2Jsecond"
        );
    }
}
