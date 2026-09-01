use thiserror::Error;

#[derive(Debug, Error)]
pub enum CompressionError {
    #[error("compressed block exceeds maximum size")]
    CompressedSizeExceeded,

    #[error("decompressed block exceeds maximum size")]
    DecompressedSizeExceeded,

    #[error("compression failed: {0}")]
    Compression(#[from] std::io::Error),

    #[error("invalid compressed block")]
    InvalidBlock,
}
