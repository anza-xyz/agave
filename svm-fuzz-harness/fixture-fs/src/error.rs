//! Filesystem management errors.

use {
    prost::{DecodeError, EncodeError},
    thiserror::Error,
};

#[derive(Debug, Error)]
pub enum FsError {
    #[error("Decode error")]
    DecodeError(#[from] DecodeError),
    #[error("Encode error")]
    EncodeError(#[from] EncodeError),
    #[error("JSON serialization error")]
    JsonSerializationError(#[from] serde_json::Error),
    #[error("File operation error")]
    FileOperationError(#[from] std::io::Error),
    #[error("Invalid file extension: {0}")]
    InvalidFileExtension(String),
}
