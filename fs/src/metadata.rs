use std::path::Path;

/// typical upper bound for filesystem block size
const DEFAULT_BLOCK_SIZE: u64 = 4096;

#[cfg(target_os = "linux")]
pub(crate) fn direct_io_offset_alignment(path: impl AsRef<Path>) -> u64 {
    // in linux kernel >6.1 we can use STATX_DIOALIGN instead
    if let Ok(metadata) = std::fs::metadata(path) {
        use std::os::linux::fs::MetadataExt;
        return metadata.st_blksize();
    }

    // fallback to default
    DEFAULT_BLOCK_SIZE
}
