/// Utilities for querying filesystem metadata, including direct I/O support.
#[cfg(all(target_os = "linux", not(target_env = "musl")))]
use std::{ffi::CString, mem};
#[cfg(target_os = "linux")]
use std::{fs, path::PathBuf};
use std::{io, path::Path};

/// Indicates whether a filesystem path supports direct I/O (opening files with `O_DIRECT` flag).
#[derive(Debug, PartialEq, Eq)]
pub enum DirectIoSupport {
    /// The filesystem does not support direct I/O.
    Unsupported,
    /// The filesystem supports direct I/O.
    Supported,
    /// Support could not be determined (e.g. check is not implemented on this platform).
    Uncertain,
}

/// Returns whether `path` (a file or directory) resides on a filesystem that supports
/// direct I/O (`O_DIRECT`).
///
/// Returns `Ok(Supported)` if direct I/O is supported, `Ok(Unsupported)` if it is not,
/// or `Ok(Uncertain)` if no conclusion can be drawn (e.g. an empty directory or a path
/// that does not exist).
///
/// On Linux: resolves a concrete file under `path` first, then attempts a `statx(2)`-based
/// check; if the kernel does not support it (< 6.1), falls back to an open-probe check.
/// On non-Linux platforms: always returns `Ok(Uncertain)`.
#[cfg(target_os = "linux")]
pub fn check_direct_io_capability(path: impl AsRef<Path>) -> io::Result<DirectIoSupport> {
    let Some(file) = find_any_file_under_path(path.as_ref())? else {
        return Ok(DirectIoSupport::Uncertain);
    };
    // statx with STATX_DIOALIGN is the preferred check, but libc does not expose
    // statx on musl (requires musl >= 1.2.3), so skip it there.
    #[cfg(not(target_env = "musl"))]
    {
        let statx_result = check_direct_io_via_statx(&file);
        if !matches!(&statx_result, Ok(DirectIoSupport::Uncertain)) {
            return statx_result;
        }
    }
    Ok(check_direct_io_via_open_probe(&file))
}

/// Always returns `Ok(Uncertain)`, since direct I/O functionality is not used on non-Linux.
#[cfg(not(target_os = "linux"))]
pub fn check_direct_io_capability(_path: impl AsRef<Path>) -> io::Result<DirectIoSupport> {
    Ok(DirectIoSupport::Uncertain)
}

/// Check direct I/O capability via `statx(2)` with `STATX_DIOALIGN`.
///
/// Returns `Ok(Supported)` when `stx_dio_mem_align != 0` (DIO supported),
/// `Ok(Unsupported)` when `STATX_DIOALIGN` is set but DIO is not supported, or
/// `Ok(Uncertain)` when the kernel did not populate `STATX_DIOALIGN` fields (kernel < 6.1).
#[cfg(all(target_os = "linux", not(target_env = "musl")))]
fn check_direct_io_via_statx(path: &Path) -> io::Result<DirectIoSupport> {
    let path_cstr = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a null byte"))?;

    let mut statx_info: libc::statx = unsafe { mem::zeroed() };
    let ret = unsafe {
        libc::statx(
            libc::AT_FDCWD,
            path_cstr.as_ptr(),
            // Follow symlinks; don't trigger automounts.
            libc::AT_NO_AUTOMOUNT,
            libc::STATX_DIOALIGN,
            &mut statx_info,
        )
    };

    if ret != 0 {
        // statx is available since 4.11, fail completely if it doesn't properly execute
        return Err(io::Error::last_os_error());
    }

    // Kernel did not populate DIOALIGN fields - kernels < 6.1 silently ignore unknown mask bits
    // and file systems without proper implementation of the check will also ignore it.
    let supported = if statx_info.stx_mask & libc::STATX_DIOALIGN == 0 {
        DirectIoSupport::Uncertain
    } else if statx_info.stx_dio_mem_align != 0 {
        // stx_dio_mem_align == 0 means the filesystem does not support DIO.
        DirectIoSupport::Supported
    } else {
        DirectIoSupport::Unsupported
    };
    Ok(supported)
}

/// Check direct I/O capability by attempting to open `file` with `O_DIRECT`.
///
/// Confirms the file is readable, then tries to reopen it with `O_DIRECT`.
/// Returns `Uncertain` if the file is not readable and no conclusion can be drawn.
#[cfg(target_os = "linux")]
fn check_direct_io_via_open_probe(file: &Path) -> DirectIoSupport {
    use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt as _};

    // Confirm the file is readable at all; if not, we cannot conclude anything.
    if OpenOptions::new().read(true).open(file).is_err() {
        return DirectIoSupport::Uncertain;
    }
    if OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(file)
        .is_ok()
    {
        DirectIoSupport::Supported
    } else {
        DirectIoSupport::Unsupported
    }
}

/// Returns a path to any regular file at or under `path`, recursively traversing
/// directories and returning as soon as one file is found. Returns `Ok(None)` if
/// no file exists under `path`.
#[cfg(target_os = "linux")]
fn find_any_file_under_path(path: &Path) -> io::Result<Option<PathBuf>> {
    if path.is_file() {
        return Ok(Some(path.to_path_buf()));
    }
    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            if let Some(path) = find_any_file_under_path(&entry.path())? {
                return Ok(Some(path));
            }
        }
    }
    Ok(None)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use {super::*, tempfile::TempDir};

    fn make_temp_file(dir: &TempDir, name: &str, content: &[u8]) -> std::path::PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn test_find_any_file_under_path_file() {
        let dir = TempDir::new().unwrap();
        let file = make_temp_file(&dir, "f.bin", b"hello");
        assert_eq!(find_any_file_under_path(&file).unwrap(), Some(file));
    }

    #[test]
    fn test_find_any_file_under_path_dir() {
        let dir = TempDir::new().unwrap();
        make_temp_file(&dir, "a.bin", b"data");
        let candidate = find_any_file_under_path(dir.path()).unwrap();
        assert!(candidate.is_some());
        assert!(candidate.unwrap().is_file());
    }

    #[test]
    fn test_find_any_file_under_path_empty_dir() {
        let dir = TempDir::new().unwrap();
        assert_eq!(find_any_file_under_path(dir.path()).unwrap(), None);
    }

    #[test]
    fn test_path_supports_direct_io_file() {
        let dir = TempDir::new().unwrap();
        let file = make_temp_file(&dir, "probe.bin", &[0u8; 4096]);
        let result = check_direct_io_capability(&file).expect("check must not fail");
        assert_eq!(
            result,
            DirectIoSupport::Supported,
            "dev filesystem must support direct I/O"
        );
    }

    #[test]
    fn test_path_supports_direct_io_dir() {
        let dir = TempDir::new().unwrap();
        make_temp_file(&dir, "probe.bin", &[0u8; 4096]);
        let result = check_direct_io_capability(dir.path()).expect("check must not fail");
        assert_eq!(
            result,
            DirectIoSupport::Supported,
            "dev filesystem must support direct I/O"
        );
    }

    #[test]
    fn test_path_supports_direct_io_empty_dir() {
        let dir = TempDir::new().unwrap();
        let result = check_direct_io_capability(dir.path()).expect("check must not fail");
        assert_eq!(result, DirectIoSupport::Uncertain);
    }

    #[test]
    fn test_path_supports_direct_io_nonexistent_path() {
        let dir = TempDir::new().unwrap();
        let result = check_direct_io_capability(dir.path().join("does-not-exist"))
            .expect("check must not fail");
        assert_eq!(result, DirectIoSupport::Uncertain);
    }
}
