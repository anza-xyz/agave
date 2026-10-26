use std::{
    fmt,
    io::{self, Write},
};

pub fn write_stdout(args: fmt::Arguments<'_>) -> io::Result<()> {
    io::stdout().lock().write_fmt(args)
}

pub fn write_stdout_str(s: &str) -> io::Result<()> {
    io::stdout().lock().write_all(s.as_bytes())
}

pub fn writeln_stdout(args: fmt::Arguments<'_>) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_fmt(args)?;
    stdout.write_all(b"\n")
}
