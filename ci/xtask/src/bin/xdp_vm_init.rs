use {
    anyhow::{Context, Result, bail},
    std::{fs, os::unix::process::ExitStatusExt, path::PathBuf, process::Command},
};

fn main() {
    if let Err(err) = try_main() {
        eprintln!("init: failure: {err:?}");
        poweroff();
        std::process::exit(1);
    }
}

fn try_main() -> Result<()> {
    // Minimal filesystem setup for the Rust test binaries.
    for directory in ["/proc", "/dev", "/tmp", "/sys"] {
        fs::create_dir_all(directory).with_context(|| format!("failed to create {directory}"))?;
    }
    mount("proc", "/proc", "proc", 0)?;
    mount("sysfs", "/sys", "sysfs", 0)?;
    mount("devtmpfs", "/dev", "devtmpfs", 0).ok();
    mount("tmpfs", "/tmp", "tmpfs", 0).ok();

    // Safety: the init process is still single-threaded here.
    unsafe {
        std::env::set_var("PATH", "/usr/sbin:/usr/bin:/sbin:/bin");
        std::env::set_var("RUST_BACKTRACE", "1");
    }

    // veth is usually modular in distro kernels. Tests still pass if it is builtin.
    let _ = Command::new("/bin/busybox")
        .args(["modprobe", "veth"])
        .status();

    let run_args = guest_run_args()?;
    let tests = discover_tests("/tests")?;
    if tests.is_empty() {
        bail!("no test binaries staged in /tests");
    }

    for test in tests {
        eprintln!("init: running {}", test.display());
        let status = Command::new(&test)
            .args(&run_args)
            .status()
            .with_context(|| format!("failed to run {}", test.display()))?;
        if !status.success() {
            bail!("{} exited with {}", test.display(), render_status(&status),);
        }
    }

    eprintln!("init: success");
    poweroff();
    Ok(())
}

fn guest_run_args() -> Result<Vec<String>> {
    let cmdline = fs::read_to_string("/proc/cmdline").context("failed to read /proc/cmdline")?;
    Ok(cmdline
        .split_whitespace()
        .filter_map(|arg| arg.strip_prefix("init.arg="))
        .map(str::to_owned)
        .collect())
}

fn discover_tests(dir: &str) -> Result<Vec<PathBuf>> {
    let mut tests = fs::read_dir(dir)
        .with_context(|| format!("failed to read {dir}"))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    tests.sort();
    Ok(tests)
}

fn mount(source: &str, target: &str, fstype: &str, flags: libc::c_ulong) -> Result<()> {
    let source = std::ffi::CString::new(source)?;
    let target = std::ffi::CString::new(target)?;
    let fstype = std::ffi::CString::new(fstype)?;
    let result = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            flags,
            std::ptr::null(),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to mount {fstype:?} on {target:?}"))
    }
}

fn render_status(status: &std::process::ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("exit code {code}");
    }
    if let Some(signal) = status.signal() {
        return format!("signal {signal}");
    }
    "unknown status".to_string()
}

fn poweroff() {
    let _ = Command::new("/bin/busybox")
        .args(["poweroff", "-f"])
        .status();
    let _ = Command::new("/bin/busybox").args(["halt", "-f"]).status();
    std::thread::sleep(std::time::Duration::from_secs(2));
}
