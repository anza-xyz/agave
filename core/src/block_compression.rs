use bg_compression::{
    compress,
    CompressionConfig,
    CompressionError,
};

/// Compresses a finalized block representation for transport/storage.
///
/// IMPORTANT:
/// - Do NOT hash the compressed bytes as the consensus block hash.
/// - Do NOT replace normal Agave shreds with this directly.
/// - The original block remains the canonical consensus representation.
///
/// This module exists so networking/storage layers can opt into compressed
/// transport without modifying transaction execution.
pub struct BlockCompressor {
    config: CompressionConfig,
}

impl Default for BlockCompressor {
    fn default() -> Self {
        Self {
            config: CompressionConfig::default(),
        }
    }
}

impl BlockCompressor {
    pub fn new(config: CompressionConfig) -> Self {
        Self { config }
    }

    pub fn compress_block(
        &self,
        block: &[u8],
    ) -> Result<Vec<u8>, CompressionError> {
        compress(block, &self.config)
    }

    pub fn config(&self) -> &CompressionConfig {
        &self.config
    }
}
