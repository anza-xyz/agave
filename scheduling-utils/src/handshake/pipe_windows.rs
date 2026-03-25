use {
    sha2::{Digest, Sha256},
    std::{ffi::OsStr, os::windows::ffi::OsStrExt, path::Path},
};

pub(super) fn pipe_name(path: &Path) -> Vec<u16> {
    let mut hasher = Sha256::new();
    for unit in path.as_os_str().encode_wide() {
        hasher.update(unit.to_le_bytes());
    }

    let digest = hasher.finalize();
    let pipe_name = format!(r"\\.\pipe\agave-scheduler-bindings-{digest:x}");

    OsStr::new(&pipe_name)
        .encode_wide()
        .chain(core::iter::once(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::pipe_name;
    use std::path::Path;

    #[test]
    fn pipe_name_is_deterministic_for_same_path() {
        let path = Path::new(r"C:\tmp\agave.sock");

        assert_eq!(pipe_name(path), pipe_name(path));
    }

    #[test]
    fn pipe_name_changes_with_path() {
        assert_ne!(
            pipe_name(Path::new(r"C:\tmp\agave-a.sock")),
            pipe_name(Path::new(r"C:\tmp\agave-b.sock"))
        );
    }
}
