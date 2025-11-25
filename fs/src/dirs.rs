use std::{
    fs, io,
    path::{Path, PathBuf},
};
#[cfg(target_os = "linux")]
use {crate::io_uring::dir_remover::RingDirRemover, agave_io_uring::io_uring_supported};

/// Removes a directory and all its contents.
pub fn remove_dir_all(path: impl Into<PathBuf> + AsRef<Path>) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        assert!(io_uring_supported());
        if let Ok(mut remover) = RingDirRemover::new() {
            return remover.remove_dir_all(path);
        }
    }

    fs::remove_dir_all(path)
}

/// Removes the contents of a directory, but not the directory itself.
pub fn remove_dir_contents(path: impl AsRef<Path>) {
    let path = path.as_ref();

    #[cfg(target_os = "linux")]
    {
        assert!(io_uring_supported());
        if let Ok(mut remover) = RingDirRemover::new() {
            if let Err(e) = remover.remove_dir_contents(path) {
                log::warn!("Failed to delete contents of '{}': {e}", path.display());
            }

            return;
        }
    }

    remove_dir_contents_slow(path)
}

/// Delete the files and subdirectories in a directory.
/// This is useful if the process does not have permission
/// to delete the top level directory it might be able to
/// delete the contents of that directory.
fn remove_dir_contents_slow(path: impl AsRef<Path>) {
    match fs::read_dir(&path) {
        Err(err) => {
            log::warn!(
                "Failed to delete contents of '{}': could not read dir: {err}",
                path.as_ref().display(),
            )
        }
        Ok(dir_entries) => {
            for entry in dir_entries.flatten() {
                let sub_path = entry.path();
                let result = if sub_path.is_dir() {
                    fs::remove_dir_all(&sub_path)
                } else {
                    fs::remove_file(&sub_path)
                };
                if let Err(err) = result {
                    log::warn!(
                        "Failed to delete contents of '{}': {err}",
                        sub_path.display(),
                    );
                }
            }
        }
    }
}
