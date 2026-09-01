#[derive(Debug, Clone)]
pub struct CompressionConfig {
    /// Zstandard compression level.
    ///
    /// 3 is a good default for validator/network workloads.
    pub level: i32,

    /// Maximum compressed block size we allow.
    pub max_compressed_size: usize,

    /// Maximum decompressed block size.
    pub max_decompressed_size: usize,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            level: 3,

            // 10 MiB compressed transport target.
            max_compressed_size: 10 * 1024 * 1024,

            // Allow considerably more expansion protection.
            max_decompressed_size: 256 * 1024 * 1024,
        }
    }
}
