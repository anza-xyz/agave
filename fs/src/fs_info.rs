use std::path::Path;

const DEFAULT_BLOCK_SIZE: u64 = 4096; // Common block size for many filesystems

pub(crate) fn get_block_size(path: impl AsRef<Path>) -> u64 {
    // Try platform-specific methods first
    #[cfg(target_os = "linux")]
    {
        if let Ok(metadata) = std::fs::metadata(path) {
            use std::os::linux::fs::MetadataExt;
            return metadata.st_blksize();
        }
    }

    // Fallback to default
    DEFAULT_BLOCK_SIZE
}
